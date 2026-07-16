;; Shared normalization for providers exposing the Responses protocol.

(define responses-stream-rules
  (list
    (hash 'match (hash "/type" "response.output_text.delta")
          'emit "model_delta" 'value "/delta")
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
    [else (error! "unsupported normalized message")]))

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

(define (responses-usage events)
  (if (null? events)
      #f
      (let ([event (car events)])
        (if (equal? (hash-ref event 'type) "response.completed")
            (hash-ref (hash-ref event 'response) 'usage)
            (responses-usage (cdr events))))))

(define (responses-preserved-items provider events)
  (if (null? events)
      '()
      (let* ([event (car events)]
             [item (hash-try-get event 'item)]
             [type (if item (hash-ref item 'type) "")]
             [rest (responses-preserved-items provider (cdr events))])
        (if (and (equal? (hash-ref event 'type) "response.output_item.done")
                 (or (equal? type "reasoning")
                     (equal? type "compaction")
                     (equal? type "web_search_call")))
            (cons (hash 'kind "provider_item" 'provider provider 'item item)
                  rest)
            rest))))

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
