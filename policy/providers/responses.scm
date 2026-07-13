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
            'strict #t
            'parameters (hash-ref spec 'parameters))))

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

(define (responses-call events)
  (if (null? events)
      #f
      (let* ([event (car events)]
             [item (hash-try-get event 'item)])
        (if (and (equal? (hash-ref event 'type) "response.output_item.done")
                 item
                 (equal? (hash-ref item 'type) "function_call"))
            (hash 'kind "tool_call"
                  'call_id (hash-ref item 'call_id)
                  'name (hash-ref item 'name)
                  'arguments (hash-ref item 'arguments))
            (responses-call (cdr events))))))

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
