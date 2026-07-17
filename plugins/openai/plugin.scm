;; OpenAI-specific behavior over the kernel's generic HTTP/SSE effect.

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
          'context_window 272000
          'compaction_token_limit 244800
          'strict_json_schema_capable #t
          'function_tools #t
          'hosted_tools (list "openai/hosted-web-search")
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
  (define history
    (responses-complete-tool-history (hash-ref prompt 'messages)))
  (define base-body
    (hash 'model model
          'instructions (hash-ref prompt 'instructions)
          'input (responses-input-items "openai" history)
          'tools (map responses-tool (hash-ref prompt 'tools))
          'prompt_cache_key (runtime-session-id)
          'tool_choice "auto"
          'parallel_tool_calls #t
          'reasoning (hash 'effort reasoning 'summary "concise"
                           'context "all_turns")
          'service_tier service-tier
          'store #f
          'stream #t
          'include (list "reasoning.encrypted_content"
                         "web_search_call.action.sources")))
  (define output-schema (hash-try-get prompt 'output_schema))
  (define body
    (if output-schema
        (hash-insert base-body 'text
                     (responses-structured-text "phi_compaction" output-schema))
        base-body))
  (hash 'type "http_request"
        'url "https://chatgpt.com/backend-api/codex/responses"
        'secret "openai_chatgpt"
        'headers (hash 'originator "codex_cli_rs"
                       'user-agent "codex_cli_rs/0.144.1"
                       'session_id (runtime-session-id))
        'timeout_ms 120000
        'stream responses-stream-rules
        'body (cond [(equal? service-tier "default")
                     (hash-remove body 'service_tier)]
                    [(equal? service-tier "fast")
                     (hash-insert body 'service_tier "priority")]
                    [else body])))

(register-provider!
  "openai" provider-effect responses-calls responses-arguments responses-output
  responses-usage
  (lambda (events) (responses-preserved-items "openai" events))
  responses-message-phase)
