(set-agent-instructions!
  "You are a coding agent running inside a Phi harness in the user's current workspace. Work directly on the user's requests using the available tools. Inspect before editing, verify changes, and continue until the requested outcome is complete. Keep responses concise. When working on or reconfiguring the Phi harness itself, read the phi-harness skill with read_file before acting. The user can already see tool calls and their output, so do not repeat them verbatim; state only the conclusion or necessary interpretation. When reconfiguring Phi, edit the active config.scm, validate the change, and reload it into the current conversation.")

(define active-context-items '())
(define next-context-span 2)
(define next-context-summary 1)
(define pending-context (hash))

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
                "ready" "" 0)))

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
  (set! active-context-items
        (or (hash-try-get state 'context_items)
            (context-initial-items messages)))
  (set! next-context-span (or (hash-try-get state 'next_context_span) 2))
  (set! next-context-summary (or (hash-try-get state 'next_context_summary) 1))
  (set! pending-context (or (hash-try-get state 'pending_context) (hash)))
  (define compactions (hash-ref state 'compactions))
  (define last-usage (hash-ref state 'last_usage))
  (define model (hash-ref state 'model))
  (define context-budget
    (hash-ref (model-spec model) 'compaction_token_limit))
  (define reasoning (hash-ref state 'reasoning))
  (define service-tier (hash-ref state 'service_tier))
  (define activity (hash-ref state 'activity))
  (define pending-finish (hash-ref state 'pending_finish))
  (define compaction-attempt
    (or (hash-try-get state 'compaction_attempt) 0))
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
                         model reasoning service-tier "working" "" 0))
       (request-effect messages model reasoning service-tier)]
      [(equal? event-type "compact_requested")
       (if (null? messages)
           (begin
             (set! next-state
                   (make-state messages compactions last-usage context-budget
                               model reasoning service-tier "ready" "" 0))
             (hash 'type "finish" 'content "Nothing to compact."))
           (begin
             (set! next-state
                   (make-state messages compactions last-usage context-budget
                               model reasoning service-tier "compacting"
                               "Compaction complete." 0))
             (start-selected-compaction messages context-budget)))]
      [(equal? event-type "model_selected")
       (set! model (hash-ref event 'model))
       (set! context-budget
             (hash-ref (model-spec model) 'compaction_token_limit))
       (set! reasoning (hash-ref event 'reasoning))
       (set! service-tier (hash-ref event 'service_tier))
       (set! next-state
             (make-state messages compactions last-usage
                         context-budget model reasoning service-tier "ready" "" 0))
       (hash 'type "finish" 'content
             (string-append "Model set to " model " " reasoning
                            " " service-tier))]
      [(equal? event-type "http_completed")
       (cond
         [(not (hash-ref event 'success))
          (if (equal? activity "selective_compacting")
              (let* ([call-id (hash-ref pending-context 'call_id)]
                     [result (hash 'error (hash-ref event 'error))])
                (set! messages
                      (append messages
                              (list (hash 'kind "tool_result" 'call_id call-id
                                          'content (value->jsexpr-string result)))))
                (set! pending-context (hash))
                (set! next-state
                      (make-state messages compactions last-usage context-budget
                                  model reasoning service-tier "working" "" 0))
                (request-effect messages model reasoning service-tier))
              (begin
                (set! next-state
                      (make-state messages compactions last-usage context-budget
                                  model reasoning service-tier "ready" "" 0))
                (hash 'type "finish" 'content (hash-ref event 'error))))]
         [(equal? activity "selective_compacting")
          (define ids (hash-ref pending-context 'items))
          (define selected (context-selection active-context-items ids))
          (define summary-text (complete-context-summary (hash-ref event 'events) model))
          (if (equal? summary-text "")
              (if (< compaction-attempt 4)
                  (begin
                    (set! next-state
                          (make-state messages compactions last-usage context-budget
                                      model reasoning service-tier
                                      "selective_compacting" ""
                                      (+ compaction-attempt 1)))
                    (start-context-summary-repair
                      (context-selected-messages selected)
                      model reasoning service-tier))
                  (let* ([call-id (hash-ref pending-context 'call_id)]
                         [result
                          (hash 'error
                                "selective context compactor returned no summary after 4 repair attempts")])
                    (set! messages
                          (append messages
                                  (list (hash 'kind "tool_result" 'call_id call-id
                                              'content (value->jsexpr-string result)))))
                    (set! pending-context (hash))
                    (set! next-state
                          (make-state messages compactions last-usage context-budget
                                      model reasoning service-tier "working" "" 0))
                    (request-effect messages model reasoning service-tier)))
              (begin
                (define summary-message
                  (hash 'kind "message" 'role "user"
                        'content (string-append "Context summary:\n" summary-text)))
                (define summary-id
                  (context-runtime-id "C" next-context-summary))
                (define summary
                  (context-summary-item
                    summary-id (hash-ref pending-context 'label) summary-message
                    (context-sum-from-tokens selected) ids selected
                    (context-selection-after selected)))
                (set! next-context-summary (+ next-context-summary 1))
                (set! active-context-items
                      (context-replace-selection active-context-items ids summary))
                (set! messages (context-flatten-items active-context-items))
                (define result
                  (hash 'created (context-public-item summary)
                        'usage
                        (hash-ref
                          (context-inspection active-context-items messages last-usage
                                              (hash-ref (model-spec model) 'context_window))
                          'usage)))
                (set! messages
                      (append messages
                              (list (hash 'kind "tool_result"
                                          'call_id (hash-ref pending-context 'call_id)
                                          'content (value->jsexpr-string result)))))
                (set! pending-context (hash))
                (set! compactions (+ compactions 1))
                (set! next-state
                      (make-state messages compactions last-usage context-budget
                                  model reasoning service-tier "working" "" 0))
                (request-effect messages model reasoning service-tier)))]
         [(equal? activity "compacting")
          (define result
            (complete-selected-compaction
              messages last-usage context-budget (hash-ref event 'events)
              compaction-attempt))
          (define retry (hash-try-get result 'retry))
          (if retry
              (begin
                (set! next-state
                      (make-state messages compactions last-usage
                                  context-budget model reasoning service-tier
                                  "compacting" pending-finish
                                  (+ compaction-attempt 1)))
                retry)
              (begin
                (set! messages (hash-ref result 'messages))
                (set! compactions (+ compactions 1))
                (if (equal? pending-finish "")
                    (begin
                      (set! next-state
                            (make-state messages compactions last-usage
                                        context-budget model reasoning service-tier
                                        "working" "" 0))
                      (request-effect messages model reasoning service-tier))
                    (begin
                      (set! next-state
                            (make-state messages compactions last-usage
                                        context-budget model reasoning service-tier
                                        "ready" "" 0))
                      (hash 'type "finish" 'content pending-finish)))))]
         [else
          (define events (hash-ref event 'events))
          (define calls (provider-calls-for model events))
          (define usage (provider-usage-for model events))
          (define preserved (provider-preserved-items-for model events))
          (if usage (set! last-usage usage))
          (set! messages (append messages preserved))
          (if (not (null? calls))
              (if (and (null? (cdr calls))
                       (context-tool-name? (hash-ref (car calls) 'name)))
                  (let* ([call (car calls)]
                         [name (hash-ref call 'name)]
                         [arguments (provider-arguments-for model call)])
                    (set! messages (append messages calls))
                    (set! active-context-items
                          (context-sync-items active-context-items messages))
                    (cond
                      [(equal? name "context_inspect")
                       (define result
                         (context-inspection
                           active-context-items messages last-usage
                           (hash-ref (model-spec model) 'context_window)))
                       (set! messages
                             (append messages (list (context-tool-result call result))))
                       (set! next-state
                             (make-state messages compactions last-usage
                                         context-budget model reasoning service-tier
                                         "working" "" 0))
                       (request-effect messages model reasoning service-tier)]
                      [(equal? name "context_mark")
                       (define label (hash-ref arguments 'label))
                       (define result
                         (hash 'closed (hash-ref (context-last-item active-context-items) 'id)
                               'opened (context-runtime-id "S" next-context-span)
                               'label label))
                       (set! messages
                             (append messages (list (context-tool-result call result))))
                       (set! active-context-items
                             (context-sync-items active-context-items messages))
                       (set! active-context-items
                             (context-mark-items
                               active-context-items
                               (context-runtime-id "S" next-context-span)
                               label (hash-ref call 'call_id)))
                       (set! next-context-span (+ next-context-span 1))
                       (set! next-state
                             (make-state messages compactions last-usage
                                         context-budget model reasoning service-tier
                                         "working" "" 0))
                       (request-effect messages model reasoning service-tier)]
                      [else
                       (define ids (hash-ref arguments 'items))
                       (define selected (context-selection active-context-items ids))
                       (set! pending-context
                             (hash 'call_id (hash-ref call 'call_id)
                                   'items ids
                                   'label (hash-ref arguments 'label)))
                       (set! next-state
                             (make-state messages compactions last-usage
                                         context-budget model reasoning service-tier
                                         "selective_compacting" "" 0))
                       (start-context-summary
                         (context-selected-messages selected)
                         model reasoning service-tier)]))
                  (begin
                (set! messages (append messages calls))
                (set! next-state
                      (make-state messages compactions last-usage
                                  context-budget model reasoning service-tier
                                  "working" "" 0))
                (hash 'type "run_tools"
                      'calls (map (lambda (call)
                                    (tool-call-execution model call))
                                  calls))))
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
                                        "compacting" finished-content 0))
                      (start-selected-compaction messages context-budget))
                    (begin
                      (set! next-state
                            (make-state messages compactions last-usage
                                        context-budget model reasoning service-tier
                                        "ready" "" 0))
                      (hash 'type "finish" 'content finished-content)))))])]
      [(equal? event-type "tools_completed")
       (set! messages
             (append messages
                     (map tool-result-message (hash-ref event 'results))))
       (if (selected-compaction-needed? messages last-usage context-budget)
           (begin
             (set! next-state
                   (make-state messages compactions last-usage context-budget
                               model reasoning service-tier "compacting" "" 0))
             (start-selected-compaction messages context-budget))
           (begin
             (set! next-state
                   (make-state messages compactions last-usage context-budget
                               model reasoning service-tier "working" "" 0))
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
                    model reasoning service-tier activity pending-finish
                    compaction-attempt)
  (define usage
    (if (and (hash-try-get last-usage 'total_tokens)
             (not (hash-try-get last-usage '_message_tokens)))
        (hash-insert last-usage '_message_tokens
                     (estimated-message-tokens messages))
        last-usage))
  (set! active-context-items
        (context-sync-items active-context-items messages))
  (hash 'messages messages
        'estimated_tokens (estimated-context-tokens messages usage)
        'compactions compactions
        'last_usage usage
        'model model
        'reasoning reasoning
        'service_tier service-tier
        'activity activity
        'pending_finish pending-finish
        'compaction_attempt compaction-attempt
        'context_items active-context-items
        'next_context_span next-context-span
        'next_context_summary next-context-summary
        'pending_context pending-context
        'context_window (hash-ref (model-spec model) 'context_window)))

(load-plugin! "responses")
(load-plugin! "openai")
(load-plugin! "openrouter")
(load-plugin! "openai-web-search")
(load-plugin! "openrouter-web-search")
(load-plugin! "skills")
(load-plugin! "context-management")
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
        'retain_token_limit 24000
        'max_repair_attempts 4))
