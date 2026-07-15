(set-agent-instructions!
  "You are a coding agent running inside a Phi harness in the user's current workspace. Work directly on the user's requests using the available tools. Inspect before editing, verify changes, and continue until the requested outcome is complete. Keep responses concise. When working on or reconfiguring the Phi harness itself, load the phi-harness skill before acting. The user can already see tool calls and their output, so do not repeat them verbatim; state only the conclusion or necessary interpretation. When reconfiguring Phi, edit the active config.scm, validate the change, and reload it into the current conversation.")

(define (init encoded-config)
  (define config (string->jsexpr encoded-config))
  (define model (or (hash-try-get config 'model) ""))
  (define context-budget
    (hash-ref (model-spec model) 'compaction_token_limit))
  (value->jsexpr-string
    (make-state '() 0 (hash) context-budget
                model
                (or (hash-try-get config 'reasoning) "")
                (or (hash-try-get config 'service_tier) "")
                "ready" "")))

(define (tool-call-execution model call)
  (define implementation
    (callable-tool-for model (hash-ref call 'name)))
  (if implementation
      (let* ([arguments (provider-arguments-for model call)]
             [request (start-callable-tool implementation arguments)])
        (hash-insert
          (hash-insert
            (hash-insert
              (hash-insert
                (hash-insert
                  (hash-insert request 'mode "http")
                  'call_id (hash-ref call 'call_id))
                'name (hash-ref call 'name))
              'arguments arguments)
            'implementation (hash-ref implementation 'name))
          'parallel (hash-ref implementation 'parallel)))
      (hash 'mode "direct"
            'call_id (hash-ref call 'call_id)
            'name (hash-ref call 'name)
            'arguments (hash-ref call 'arguments))))

(define (tool-result-message result)
  (hash 'kind "tool_result"
        'call_id (hash-ref result 'call_id)
        'content (value->jsexpr-string (hash-ref result 'result))))

(define (on-event encoded-state encoded-event)
  (define state (string->jsexpr encoded-state))
  (define event (string->jsexpr encoded-event))
  (define event-type (hash-ref event 'type))
  (define messages (hash-ref state 'messages))
  (define compactions (hash-ref state 'compactions))
  (define last-usage (hash-ref state 'last_usage))
  (define model (hash-ref state 'model))
  (define context-budget
    (hash-ref (model-spec model) 'compaction_token_limit))
  (define reasoning (hash-ref state 'reasoning))
  (define service-tier (hash-ref state 'service_tier))
  (define activity (hash-ref state 'activity))
  (define pending-finish (hash-ref state 'pending_finish))
  (define next-state state)
  (define effect
    (cond
      [(equal? event-type "user_message")
       (set! messages
             (append messages
                     (list (hash 'kind "message" 'role "user"
                                 'content (hash-ref event 'content)))))
       (set! next-state
             (make-state messages compactions last-usage context-budget
                         model reasoning service-tier "working" ""))
       (request-effect messages model reasoning service-tier)]
      [(equal? event-type "compact_requested")
       (if (null? messages)
           (begin
             (set! next-state
                   (make-state messages compactions last-usage context-budget
                               model reasoning service-tier "ready" ""))
             (hash 'type "finish" 'content "Nothing to compact."))
           (begin
             (set! next-state
                   (make-state messages compactions last-usage context-budget
                               model reasoning service-tier "compacting"
                               "Compaction complete."))
             (start-selected-compaction messages context-budget)))]
      [(equal? event-type "model_selected")
       (set! model (hash-ref event 'model))
       (set! context-budget
             (hash-ref (model-spec model) 'compaction_token_limit))
       (set! reasoning (hash-ref event 'reasoning))
       (set! service-tier (hash-ref event 'service_tier))
       (set! next-state
             (make-state messages compactions last-usage
                         context-budget model reasoning service-tier "ready" ""))
       (hash 'type "finish" 'content
             (string-append "Model set to " model " · " reasoning
                            " · " service-tier))]
      [(equal? event-type "http_completed")
       (cond
         [(not (hash-ref event 'success))
          (set! next-state
                (make-state messages compactions last-usage context-budget
                            model reasoning service-tier "ready" ""))
          (hash 'type "finish" 'content (hash-ref event 'error))]
         [(equal? activity "compacting")
          (set! messages
                (complete-selected-compaction
                  messages last-usage context-budget (hash-ref event 'events)))
          (set! compactions (+ compactions 1))
          (if (equal? pending-finish "")
              (begin
                (set! next-state
                      (make-state messages compactions last-usage
                                  context-budget model reasoning service-tier
                                  "working" ""))
                (request-effect messages model reasoning service-tier))
              (begin
                (set! next-state
                      (make-state messages compactions last-usage
                                  context-budget model reasoning service-tier
                                  "ready" ""))
                (hash 'type "finish" 'content pending-finish)))]
         [else
          (define events (hash-ref event 'events))
          (define calls (provider-calls-for model events))
          (define usage (provider-usage-for model events))
          (define preserved (provider-preserved-items-for model events))
          (if usage (set! last-usage usage))
          (set! messages (append messages preserved))
          (if (not (null? calls))
              (begin
                (set! messages (append messages calls))
                (set! next-state
                      (make-state messages compactions last-usage
                                  context-budget model reasoning service-tier
                                  "working" ""))
                (hash 'type "run_tools"
                      'calls (map (lambda (call)
                                    (tool-call-execution model call))
                                  calls)))
              (let* ([content (provider-output-for model events)]
                     [finished-content (if (equal? content "")
                                           "Model returned no output."
                                           content)]
                     [phase (provider-message-phase-for model events)])
                (set! messages
                      (append messages
                              (list (if phase
                                        (hash 'kind "message" 'role "assistant"
                                              'content content 'phase phase)
                                        (hash 'kind "message" 'role "assistant"
                                              'content content)))))
                (if (selected-compaction-needed? messages last-usage context-budget)
                    (begin
                      (set! next-state
                            (make-state messages compactions last-usage
                                        context-budget model reasoning service-tier
                                        "compacting" finished-content))
                      (start-selected-compaction messages context-budget))
                    (begin
                      (set! next-state
                            (make-state messages compactions last-usage
                                        context-budget model reasoning service-tier
                                        "ready" ""))
                      (hash 'type "finish" 'content finished-content)))))])]
      [(equal? event-type "tools_completed")
       (set! messages
             (append messages
                     (map tool-result-message (hash-ref event 'results))))
       (if (selected-compaction-needed? messages last-usage context-budget)
           (begin
             (set! next-state
                   (make-state messages compactions last-usage context-budget
                               model reasoning service-tier "compacting" ""))
             (start-selected-compaction messages context-budget))
           (begin
             (set! next-state
                   (make-state messages compactions last-usage context-budget
                               model reasoning service-tier "working" ""))
             (request-effect messages model reasoning service-tier)))]
      [else (hash 'type "finish" 'content "Unsupported event.")]))
  (value->jsexpr-string
    (hash 'state (value->jsexpr-string next-state)
          'effects (list effect))))

(define (request-effect messages model reasoning service-tier)
  (define prompt
    (build-selected-prompt messages agent-instructions (tools-for-model model)))
  (define output-schema (runtime-config-value 'output_schema #f))
  (provider-request
    (if output-schema
        (hash-insert prompt 'output_schema output-schema)
        prompt)
    model reasoning service-tier))

(define (make-state messages compactions last-usage context-budget
                    model reasoning service-tier activity pending-finish)
  (define usage
    (if (and (hash-try-get last-usage 'total_tokens)
             (not (hash-try-get last-usage '_message_tokens)))
        (hash-insert last-usage '_message_tokens
                     (estimated-message-tokens messages))
        last-usage))
  (hash 'messages messages
        'estimated_tokens (estimated-context-tokens messages usage)
        'compactions compactions
        'last_usage usage
        'model model
        'reasoning reasoning
        'service_tier service-tier
        'activity activity
        'pending_finish pending-finish
        'context_window (hash-ref (model-spec model) 'context_window)))

(load-plugin! "responses")
(load-plugin! "openai")
(load-plugin! "openrouter")
(load-plugin! "openai-web-search")
(load-plugin! "openrouter-web-search")
(load-plugin! "skills")
(load-plugin! "dynamic-workflows")
(load-plugin! "codex-patch")
(load-plugin! "simple-prompt")
(load-plugin! "compaction-structured")

(select-prompt-builder! "simple")
(select-file-editor! "codex-patch")
(configure-tool! "openai/hosted-web-search" (hash))
(configure-tool!
  "openai/callable-web-search"
  (hash 'model "openai/gpt-5.6-luna"
        'reasoning "low"
        'service_tier "default"
        'search (hash)))
(configure-tool!
  "openrouter/hosted-web-search"
  (hash 'engine "native"))
(select-tool!
  "web_search"
  (list (hash 'prefer "same-route-hosted")
        (hash 'use "openai/callable-web-search")))
(select-compactor!
  "structured"
  (hash 'model "openai/gpt-5.6-luna"
        'reasoning "low"
        'service_tier "default"
        'retain_messages 16
        'retain_token_limit 24000))
