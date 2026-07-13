(define (init encoded-config)
  (define config (string->jsexpr encoded-config))
  (value->jsexpr-string
    (hash 'messages '() 'pending_call "" 'estimated_tokens 0
          'compactions 0 'last_usage (hash)
          'context_char_budget (hash-ref config 'context_char_budget))))

(define (on-event encoded-state encoded-event)
  (define state (string->jsexpr encoded-state))
  (define event (string->jsexpr encoded-event))
  (define event-type (hash-ref event 'type))
  (define messages (hash-ref state 'messages))
  (define pending-call (hash-ref state 'pending_call))
  (define compactions (hash-ref state 'compactions))
  (define last-usage (hash-ref state 'last_usage))
  (define context-budget (hash-ref state 'context_char_budget))
  (define next-state state)
  (define effect
    (cond
      [(equal? event-type "user_message")
       (set! messages
             (append messages
                     (list (hash 'kind "message" 'role "user"
                                 'content (hash-ref event 'content)))))
       (define compacted (compact-messages messages context-budget))
       (if (< (length compacted) (length messages))
           (set! compactions (+ compactions 1)))
       (set! messages compacted)
       (set! next-state
             (make-state messages "" compactions last-usage context-budget))
       (provider-effect messages)]
      [(equal? event-type "http_completed")
       (if (not (hash-ref event 'success))
           (hash 'type "finish" 'content (hash-ref event 'error))
           (let ([call (provider-call (hash-ref event 'events))]
                 [usage (provider-usage (hash-ref event 'events))])
             (if usage (set! last-usage usage))
             (if call
                 (begin
                   (set! messages (append messages (list call)))
                   (set! next-state
                         (make-state messages (hash-ref call 'call_id)
                                     compactions last-usage context-budget))
                   (hash 'type "run_tool"
                         'name (hash-ref call 'name)
                         'arguments (provider-arguments call)))
                 (let ([content (provider-output (hash-ref event 'events))])
                   (set! messages
                         (append messages
                                 (list (hash 'kind "message" 'role "assistant"
                                             'content content))))
                   (set! next-state
                         (make-state messages "" compactions
                                     last-usage context-budget))
                   (hash 'type "finish"
                         'content (if (equal? content "")
                                      "Model returned no output."
                                      content))))))]
      [(equal? event-type "tool_completed")
       (set! messages
             (append messages
                     (list (hash 'kind "tool_result"
                                 'call_id pending-call
                                 'content (value->jsexpr-string
                                            (hash-ref event 'result))))))
       (set! messages (compact-messages messages context-budget))
       (set! next-state
             (make-state messages "" compactions last-usage context-budget))
       (provider-effect messages)]
      [else (hash 'type "finish" 'content "Unsupported event.")]))
  (value->jsexpr-string
    (hash 'state (value->jsexpr-string next-state)
          'effects (list effect))))

(define (make-state messages pending-call compactions last-usage context-budget)
  (hash 'messages messages
        'pending_call pending-call
        'estimated_tokens (quotient (string-length (value->jsexpr-string messages)) 4)
        'compactions compactions
        'last_usage last-usage
        'context_char_budget context-budget))
