;; Shared normalization for providers exposing the Responses protocol.

(define responses-stream-rules
  (list
    (hash 'match (hash "/type" "response.output_item.added"
                       "/item/type" "message")
          'emit "output_phase" 'key "/output_index" 'value "/item/phase")
    (hash 'match (hash "/type" "response.output_text.delta")
          'emit "output_delta" 'key "/output_index" 'value "/delta")
    (hash 'match (hash "/type" "response.output_item.added"
                       "/item/type" "web_search_call")
          'emit "tool_started" 'name "web_search" 'value "/item")
    (hash 'match (hash "/type" "response.output_item.done"
                       "/item/type" "web_search_call")
          'emit "tool_completed" 'name "web_search" 'value "/item")))

(define (responses-tool spec)
  (if (equal? (or (hash-try-get spec 'kind) "") "hosted_tool")
      (hash-ref spec 'wire)
      (hash 'type "function"
            'name (hash-ref spec 'name)
            'description (hash-ref spec 'description)
            'strict (not (equal? (hash-try-get spec 'strict_mode) "loose"))
            'parameters (hash-ref spec 'parameters))))

(define (responses-structured-text name schema)
  (hash 'format
        (hash 'type "json_schema"
              'name name
              'strict #t
              'schema schema)))

(define (responses-tool-ids messages kind)
  (cond
    [(null? messages) '()]
    [(equal? (hash-ref (car messages) 'kind) kind)
     (cons (hash-ref (car messages) 'call_id)
           (responses-tool-ids (cdr messages) kind))]
    [else (responses-tool-ids (cdr messages) kind)]))

(define (responses-id-present? id ids)
  (cond
    [(null? ids) #f]
    [(equal? id (car ids)) #t]
    [else (responses-id-present? id (cdr ids))]))

;; Interrupted and legacy sessions can contain only one half of a tool exchange.
;; Responses providers reject the entire request unless both halves are present.
(define (responses-complete-tool-history messages)
  (define call-ids (responses-tool-ids messages "tool_call"))
  (define result-ids (responses-tool-ids messages "tool_result"))
  (define (keep remaining)
    (cond
      [(null? remaining) '()]
      [else
       (define message (car remaining))
       (define kind (hash-ref message 'kind))
       (define keep?
         (cond
           [(equal? kind "tool_call")
            (responses-id-present? (hash-ref message 'call_id) result-ids)]
           [(equal? kind "tool_result")
            (responses-id-present? (hash-ref message 'call_id) call-ids)]
           [else #t]))
       (if keep?
           (cons message (keep (cdr remaining)))
           (keep (cdr remaining)))]))
  (keep messages))

(define (responses-message->item provider message)
  (define kind (hash-ref message 'kind))
  (cond
    [(equal? kind "message")
     (define item
       (hash 'type "message"
             'role (hash-ref message 'role)
             'content
             (list (hash 'type (if (equal? (hash-ref message 'role) "assistant")
                                    "output_text"
                                    "input_text")
                         'text (hash-ref message 'content)))))
     (define phase (hash-try-get message 'phase))
     (if phase (hash-insert item 'phase phase) item)]
    [(equal? kind "tool_call")
     (hash 'type "function_call"
           'call_id (hash-ref message 'call_id)
           'name (hash-ref message 'name)
           'arguments (hash-ref message 'arguments))]
    [(equal? kind "tool_result")
     (hash 'type "function_call_output"
           'call_id (hash-ref message 'call_id)
           'output (hash-ref message 'content))]
    [(equal? kind "provider_item")
     (if (equal? (hash-ref message 'provider) provider)
         (hash-ref message 'item)
         (error! "provider item belongs to another provider"))]
    [(equal? kind "reasoning_summary") #f]
    [else (error! "unsupported normalized message")]))

(define (responses-input-items provider messages)
  (if (null? messages)
      '()
      (let ([item (responses-message->item provider (car messages))]
            [rest (responses-input-items provider (cdr messages))])
        (if item (cons item rest) rest))))

(define (responses-calls events)
  (if (null? events)
      '()
      (let* ([event (car events)]
             [item (hash-try-get event 'item)]
             [rest (responses-calls (cdr events))])
        (if (and (equal? (hash-ref event 'type) "response.output_item.done")
                 item
                 (equal? (hash-ref item 'type) "function_call"))
            (cons (hash 'kind "tool_call"
                        'call_id (hash-ref item 'call_id)
                        'name (hash-ref item 'name)
                        'arguments (hash-ref item 'arguments))
                  rest)
            rest))))

(define (responses-arguments call)
  (with-handler
    (lambda (_)
      (hash 'malformed_arguments (hash-ref call 'arguments)))
    (string->jsexpr (hash-ref call 'arguments))))

(define (responses-output events)
  (if (null? events)
      ""
      (let* ([event (car events)]
             [rest (responses-output (cdr events))])
        (if (equal? (hash-ref event 'type) "response.output_text.delta")
            (string-append (hash-ref event 'delta) rest)
            rest))))

(define (responses-message-content content)
  (if (null? content)
      ""
      (let* ([part (car content)]
             [text (or (hash-try-get part 'text) "")])
        (string-append text (responses-message-content (cdr content))))))

(define (responses-output-for-index events output-index)
  (if (null? events)
      ""
      (let* ([event (car events)]
             [event-index (hash-try-get event 'output_index)]
             [rest (responses-output-for-index (cdr events) output-index)])
        (if (and (equal? (hash-ref event 'type) "response.output_text.delta")
                 (or (not output-index) (equal? event-index output-index)))
            (string-append (hash-ref event 'delta) rest)
            rest))))

(define (responses-completed-messages events all-events)
  (if (null? events)
      '()
      (let* ([event (car events)]
             [item (hash-try-get event 'item)]
             [rest (responses-completed-messages (cdr events) all-events)])
        (if (and (equal? (hash-ref event 'type) "response.output_item.done")
                 item
                 (equal? (hash-ref item 'type) "message"))
            (let* ([completed (responses-message-content
                                (or (hash-try-get item 'content) '()))]
                   [content (if (equal? completed "")
                                (responses-output-for-index
                                  all-events (hash-try-get event 'output_index))
                                completed)]
                   [phase (hash-try-get item 'phase)]
                   [message (hash 'kind "message" 'role "assistant"
                                  'content content)])
              (cons (if phase (hash-insert message 'phase phase) message) rest))
            rest))))

(define (responses-output-messages events)
  (define completed (responses-completed-messages events events))
  (if (null? completed)
      (let* ([content (responses-output events)]
             [phase (responses-message-phase events)]
             [message (hash 'kind "message" 'role "assistant"
                            'content content)])
        (if (equal? content "")
            '()
            (list (if phase (hash-insert message 'phase phase) message))))
      completed))

(define (responses-reasoning-summary events)
  (if (null? events)
      ""
      (let* ([event (car events)]
             [rest (responses-reasoning-summary (cdr events))])
        (if (equal? (hash-ref event 'type)
                    "response.reasoning_summary_text.delta")
            (string-append (hash-ref event 'delta) rest)
            rest))))

(define (responses-usage events)
  (if (null? events)
      #f
      (let ([event (car events)])
        (if (equal? (hash-ref event 'type) "response.completed")
            (hash-ref (hash-ref event 'response) 'usage)
            (responses-usage (cdr events))))))

(define (responses-preserved-items provider events)
  (define (items remaining)
    (if (null? remaining)
        '()
        (let* ([event (car remaining)]
               [item (hash-try-get event 'item)]
               [type (if item (hash-ref item 'type) "")]
               [rest (items (cdr remaining))])
          (if (and (equal? (hash-ref event 'type) "response.output_item.done")
                   (or (equal? type "reasoning")
                       (equal? type "compaction")
                       (equal? type "web_search_call")))
              (cons (hash 'kind "provider_item" 'provider provider 'item item)
                    rest)
              rest))))
  (define summary (responses-reasoning-summary events))
  (if (equal? summary "")
      (items events)
      (append (items events)
              (list (hash 'kind "reasoning_summary" 'content summary)))))

(define (responses-message-phase events)
  (if (null? events)
      #f
      (let* ([event (car events)]
             [item (hash-try-get event 'item)])
        (if (and (equal? (hash-ref event 'type) "response.output_item.done")
                 item
                 (equal? (hash-ref item 'type) "message"))
            (hash-try-get item 'phase)
            (responses-message-phase (cdr events))))))
