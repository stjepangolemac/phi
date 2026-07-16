;; OpenRouter provider over its Responses-compatible endpoint.

(define openrouter-reasoning-options
  (list (hash 'id "high" 'description "Greater reasoning depth.")
        (hash 'id "xhigh" 'description "Extra high reasoning depth; maps to maximum effort.")))

(define (register-openrouter-model! id description default-reasoning)
  (register-model!
    "openrouter"
    (hash 'id id
          'label id
          'description description
          'context_window 1000000
          'compaction_token_limit 180000
          'strict_json_schema_capable #t
          'function_tools #t
          'hosted_tools (list "openrouter/hosted-web-search")
          'reasoning openrouter-reasoning-options
          'default_reasoning default-reasoning
          'service_tiers '()
          'default_service_tier "")))

(register-openrouter-model!
  "anthropic/claude-sonnet-4.6" "Claude Sonnet 4.6 through OpenRouter." "high")

(define (openrouter-provider-effect prompt model reasoning _service-tier)
  (define history
    (responses-complete-tool-history (hash-ref prompt 'messages)))
  (define base-body
    (hash 'model model
          'instructions (hash-ref prompt 'instructions)
          'input (responses-input-items "openrouter" history)
          'tools (map responses-tool (hash-ref prompt 'tools))
          'tool_choice "auto"
          ;; Context tools mutate policy state and must be dispatched alone.
          'parallel_tool_calls
          (not (context-tools-available? (hash-ref prompt 'tools)))
          'reasoning (hash 'effort reasoning)
          'store #f
          'stream #t))
  (define output-schema (hash-try-get prompt 'output_schema))
  (hash 'type "http_request"
        'url "https://openrouter.ai/api/v1/responses"
        'secret "openrouter"
        'headers (hash 'x-title "Phi")
        'timeout_ms 120000
        'stream responses-stream-rules
        'body
        (if output-schema
            (hash-insert base-body 'text
                         (responses-structured-text
                           "phi_compaction" output-schema))
            base-body)))

(register-provider!
  "openrouter" openrouter-provider-effect responses-calls responses-arguments
  responses-output responses-usage
  (lambda (events) (responses-preserved-items "openrouter" events))
  responses-message-phase)
