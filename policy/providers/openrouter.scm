;; OpenRouter provider over its Responses-compatible endpoint.

(define openrouter-reasoning-options
  (list (hash 'id "low" 'description "Fast responses with lighter reasoning.")
        (hash 'id "medium" 'description "Balanced reasoning depth.")
        (hash 'id "high" 'description "Greater reasoning depth.")))

(define (register-openrouter-model! id description default-reasoning)
  (register-model!
    "openrouter"
    (hash 'id id
          'label id
          'description description
          'function_tools #t
          'hosted_tools (list "openrouter/hosted-web-search")
          'reasoning openrouter-reasoning-options
          'default_reasoning default-reasoning
          'service_tiers '()
          'default_service_tier "")))

(register-openrouter-model!
  "x-ai/grok-4.5" "Grok 4.5 through OpenRouter." "medium")
(register-openrouter-model!
  "openai/gpt-5.6-luna" "GPT-5.6 Luna through OpenRouter." "low")

(define (openrouter-provider-effect prompt model reasoning _service-tier)
  (hash 'type "http_request"
        'url "https://openrouter.ai/api/v1/responses"
        'secret "openrouter"
        'headers (hash 'x-title "Phi")
        'timeout_ms 120000
        'stream responses-stream-rules
        'body
        (hash 'model model
              'instructions (hash-ref prompt 'instructions)
              'input
              (map (lambda (message)
                     (responses-message->item "openrouter" message))
                   (hash-ref prompt 'messages))
              'tools (map responses-tool (hash-ref prompt 'tools))
              'tool_choice "auto"
              'reasoning (hash 'effort reasoning)
              'store #f
              'stream #t)))

(register-provider!
  "openrouter" openrouter-provider-effect responses-call responses-arguments
  responses-output responses-usage
  (lambda (events) (responses-preserved-items "openrouter" events))
  responses-message-phase)
