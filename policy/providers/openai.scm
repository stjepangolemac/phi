;; OpenAI-specific behavior over the kernel's generic HTTP/SSE effect.

(define (provider-tool spec)
  (hash 'type "function"
        'name (hash-ref spec 'name)
        'description (hash-ref spec 'description)
        'strict #t
        'parameters (hash-ref spec 'parameters)))

(define reasoning-options
  (list (hash 'id "low" 'description "Fast responses with lighter reasoning.")
        (hash 'id "medium" 'description "Balances speed and reasoning depth for everyday tasks.")
        (hash 'id "high" 'description "Greater reasoning depth for complex problems.")
        (hash 'id "xhigh" 'description "Extra high reasoning depth for complex problems.")
        (hash 'id "max" 'description "Maximum reasoning depth for the hardest problems.")))

(define service-tier-options
  (list (hash 'id "default" 'description "Standard speed and usage.")
        (hash 'id "fast" 'description "1.5x speed, increased usage.")))

(define (register-openai-model! id description reasoning default-reasoning)
  (register-model!
    "openai"
    (hash 'id id
          'label id
          'description description
          'reasoning reasoning
          'default_reasoning default-reasoning
          'service_tiers service-tier-options
          'default_service_tier "default")))

(register-openai-model!
  "gpt-5.6-luna" "Cost-sensitive, high-volume workloads."
  reasoning-options "low")
(register-openai-model!
  "gpt-5.6-terra" "Balances intelligence and cost."
  reasoning-options "medium")
(register-openai-model!
  "gpt-5.6-sol" "Complex reasoning and coding."
  reasoning-options "medium")

(define (provider-effect prompt model reasoning service-tier)
  (define history (hash-ref prompt 'messages))
  (define body
    (hash 'model model
          'instructions (hash-ref prompt 'instructions)
          'input (map provider-message->item history)
          'tools (map provider-tool (hash-ref prompt 'tools))
          'prompt_cache_key (runtime-session-id)
          'tool_choice "auto"
          'parallel_tool_calls #f
          'reasoning (hash 'effort reasoning 'context "all_turns")
          'service_tier service-tier
          'store #f
          'stream #t
          'include (list "reasoning.encrypted_content")))
  (hash 'type "http_request"
        'url "https://chatgpt.com/backend-api/codex/responses"
        'secret "openai_chatgpt"
        'headers (hash 'originator "codex_cli_rs"
                       'user-agent "codex_cli_rs/0.144.1"
                       'session_id (runtime-session-id)
                       'x-openai-internal-codex-responses-lite "true")
        'timeout_ms 120000
        'body (cond [(equal? service-tier "default")
                     (hash-remove body 'service_tier)]
                    [(equal? service-tier "fast")
                     (hash-insert body 'service_tier "priority")]
                    [else body])))

(define (provider-call events)
  (if (null? events)
      #f
      (let* ([event (car events)]
             [item (hash-try-get event 'item)])
        (if (and (equal? (hash-ref event 'type) "response.output_item.done")
                 item
                 (equal? (hash-ref item 'type) "function_call"))
            (provider-normalize-call item)
            (provider-call (cdr events))))))

(define (provider-message->item message)
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
     (if (equal? (hash-ref message 'provider) "openai")
         (hash-ref message 'item)
         (error! "provider item belongs to another provider"))]
    [else (error! "unsupported normalized message")]))

(define (provider-preserved-items events)
  (if (null? events)
      '()
      (let* ([event (car events)]
             [item (hash-try-get event 'item)]
             [type (if item (hash-ref item 'type) "")]
             [rest (provider-preserved-items (cdr events))])
        (if (and (equal? (hash-ref event 'type) "response.output_item.done")
                 (or (equal? type "reasoning")
                     (equal? type "compaction")))
            (cons (hash 'kind "provider_item" 'provider "openai" 'item item)
                  rest)
            rest))))

(define (provider-message-phase events)
  (if (null? events)
      #f
      (let* ([event (car events)]
             [item (hash-try-get event 'item)])
        (if (and (equal? (hash-ref event 'type) "response.output_item.done")
                 item
                 (equal? (hash-ref item 'type) "message"))
            (hash-try-get item 'phase)
            (provider-message-phase (cdr events))))))

(define (provider-normalize-call item)
  (hash 'kind "tool_call"
        'call_id (hash-ref item 'call_id)
        'name (hash-ref item 'name)
        'arguments (hash-ref item 'arguments)))

(define (provider-arguments call)
  (with-handler
    (lambda (_)
      (hash 'malformed_arguments (hash-ref call 'arguments)))
    (string->jsexpr (hash-ref call 'arguments))))

(define (provider-output events)
  (if (null? events)
      ""
      (let* ([event (car events)]
             [rest (provider-output (cdr events))])
        (if (equal? (hash-ref event 'type) "response.output_text.delta")
            (string-append (hash-ref event 'delta) rest)
            rest))))

(define (provider-usage events)
  (if (null? events)
      #f
      (let ([event (car events)])
        (if (equal? (hash-ref event 'type) "response.completed")
            (hash-ref (hash-ref event 'response) 'usage)
            (provider-usage (cdr events))))))

(register-provider! "openai" provider-effect provider-call provider-arguments
                    provider-output provider-usage provider-preserved-items
                    provider-message-phase)
