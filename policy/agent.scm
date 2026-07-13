(set-agent-instructions!
  "Answer ordinary requests directly and concisely. You can read and revision-safely replace workspace files, and run allowlisted programs. Inspect before editing and verify changes. The user can already see tool calls and their output. Do not repeat tool output verbatim; state only the conclusion or necessary interpretation. Only when the user explicitly asks you to improve Phi's policy: inspect the active policy, make one small measurable improvement, submit the complete replacement with a concise hypothesis, and report the candidate id, validation, and diff for human approval. Never claim activation.")

(define (init encoded-config)
  (define config (string->jsexpr encoded-config))
  (value->jsexpr-string
    (hash 'messages '() 'pending_call "" 'estimated_tokens 0
          'compactions 0 'last_usage (hash)
          'model (default-model-id)
          'reasoning (default-model-reasoning)
          'service_tier (default-model-service-tier)
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
  (define model (or (hash-try-get state 'model) (default-model-id)))
  (define reasoning (or (hash-try-get state 'reasoning) (default-model-reasoning)))
  (define service-tier
    (or (hash-try-get state 'service_tier) (default-model-service-tier)))
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
             (make-state messages "" compactions last-usage context-budget
                         model reasoning service-tier))
       (request-effect messages model reasoning service-tier)]
      [(equal? event-type "model_selected")
       (set! model (hash-ref event 'model))
       (set! reasoning (hash-ref event 'reasoning))
       (set! service-tier (hash-ref event 'service_tier))
       (set! next-state
             (make-state messages pending-call compactions last-usage
                         context-budget model reasoning service-tier))
       (hash 'type "finish" 'content
             (string-append "Model set to " model " · " reasoning
                            " · " service-tier))]
      [(equal? event-type "http_completed")
       (if (not (hash-ref event 'success))
           (hash 'type "finish" 'content (hash-ref event 'error))
           (let ([call (provider-call (hash-ref event 'events))]
                 [usage (provider-usage (hash-ref event 'events))]
                 [preserved (provider-preserved-items (hash-ref event 'events))])
             (if usage (set! last-usage usage))
             (set! messages (append messages preserved))
             (if call
                 (begin
                   (set! messages (append messages (list call)))
                   (set! next-state
                         (make-state messages (hash-ref call 'call_id)
                                     compactions last-usage context-budget
                                     model reasoning service-tier))
                   (hash 'type "run_tool"
                         'name (hash-ref call 'name)
                         'arguments (provider-arguments call)))
                 (let ([content (provider-output (hash-ref event 'events))]
                       [phase (provider-message-phase (hash-ref event 'events))])
                   (set! messages
                         (append messages
                                 (list (if phase
                                           (hash 'kind "message" 'role "assistant"
                                                 'content content 'phase phase)
                                           (hash 'kind "message" 'role "assistant"
                                                 'content content)))))
                   (set! next-state
                         (make-state messages "" compactions
                                     last-usage context-budget model
                                     reasoning service-tier))
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
             (make-state messages "" compactions last-usage context-budget
                         model reasoning service-tier))
       (request-effect messages model reasoning service-tier)]
      [else (hash 'type "finish" 'content "Unsupported event.")]))
  (value->jsexpr-string
    (hash 'state (value->jsexpr-string next-state)
          'effects (list effect))))

(define (request-effect messages model reasoning service-tier)
  (provider-effect
    (build-prompt messages agent-instructions (registered-tools))
    model reasoning service-tier))

(define (make-state messages pending-call compactions last-usage context-budget
                    model reasoning service-tier)
  (hash 'messages messages
        'pending_call pending-call
        'estimated_tokens (quotient (string-length (value->jsexpr-string messages)) 4)
        'compactions compactions
        'last_usage last-usage
        'model model
        'reasoning reasoning
        'service_tier service-tier
        'context_char_budget context-budget))
