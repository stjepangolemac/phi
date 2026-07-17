(set-agent-instructions!
  "You are a coding agent running inside a Phi harness in the user's current workspace. Work directly on the user's requests using the available tools. Inspect before editing, verify changes, and continue until the requested outcome is complete. Keep responses concise. Use context_mark proactively after completing a substantial phase or when changing focus so older work becomes eligible for selective compaction. When context pressure rises, inspect the active context and compact substantial closed older items when that preserves focus without losing needed details. When working on or reconfiguring the Phi harness itself, read the phi-harness skill with read_file before acting. The user can already see tool calls and their output, so do not repeat them verbatim; state only the conclusion or necessary interpretation. When reconfiguring Phi, edit the active config.scm, validate the change, and reload it into the current conversation.")

(define active-context-items '())
(define next-context-span 2)
(define next-context-summary 1)
(define pending-context (hash))
(define pending-context-calls '())
(define context-jobs '())
(define next-context-job 1)
(define context-pressure-thresholds (list 25 50 75))
(define next-context-notification 25)

(define (context-terminal-status? status)
  (or (equal? status "applied") (equal? status "failed")
      (equal? status "cancelled") (equal? status "stale")))

(define (context-job-find jobs id)
  (cond
    [(null? jobs) #f]
    [(equal? (hash-ref (car jobs) 'id) id) (car jobs)]
    [else (context-job-find (cdr jobs) id)]))

(define (context-job-replace jobs id replacement)
  (cond
    [(null? jobs) '()]
    [(equal? (hash-ref (car jobs) 'id) id)
     (cons replacement (cdr jobs))]
    [else (cons (car jobs) (context-job-replace (cdr jobs) id replacement))]))

(define (context-job-update-status id status error)
  (define job (context-job-find context-jobs id))
  (if job
      (set! context-jobs
            (context-job-replace
              context-jobs id
              (hash-insert (hash-insert job 'status status) 'error error)))))

(define (context-job-public job)
  (hash 'id (hash-ref job 'id)
        'items (hash-ref job 'items)
        'label (hash-ref job 'label)
        'status (hash-ref job 'status)
        'error (or (hash-try-get job 'error) "")))

(define (context-jobs-public jobs)
  (if (null? jobs) '()
      (cons (context-job-public (car jobs))
            (context-jobs-public (cdr jobs)))))

(define (context-pending-job? job)
  (not (context-terminal-status? (hash-ref job 'status))))

(define (context-pending-job-ids jobs)
  (cond
    [(null? jobs) '()]
    [(context-pending-job? (car jobs))
     (cons (hash-ref (car jobs) 'id)
           (context-pending-job-ids (cdr jobs)))]
    [else (context-pending-job-ids (cdr jobs))]))

(define (context-any-selected? ids selected)
  (cond
    [(null? ids) #f]
    [(string-member? (car ids) selected) #t]
    [else (context-any-selected? (cdr ids) selected)]))

(define (context-reserved-item-ids jobs)
  (cond
    [(null? jobs) '()]
    [(context-pending-job? (car jobs))
     (append (hash-ref (car jobs) 'items)
             (context-reserved-item-ids (cdr jobs)))]
    [else (context-reserved-item-ids (cdr jobs))]))

(define (context-selection-reserved? ids jobs)
  (cond
    [(null? jobs) #f]
    [(and (context-pending-job? (car jobs))
          (context-any-selected? ids (hash-ref (car jobs) 'items))) #t]
    [else (context-selection-reserved? ids (cdr jobs))]))

(define (context-compatible-selection items ids snapshot)
  (define (find remaining)
    (cond [(null? remaining) #f]
          [(equal? (hash-ref (car remaining) 'id) (car ids)) remaining]
          [else (find (cdr remaining))]))
  (define (take remaining wanted selected)
    (cond
      [(null? wanted) (reverse selected)]
      [(or (null? remaining)
           (not (equal? (hash-ref (car remaining) 'id) (car wanted)))) #f]
      [else (take (cdr remaining) (cdr wanted) (cons (car remaining) selected))]))
  (define remaining (if (null? ids) #f (find items)))
  (if remaining
      (let ([selected (take remaining ids '())])
        (if (and selected (equal? selected snapshot)) selected #f))
      #f))

(define (context-cancel-pending! reason)
  (define ids (context-pending-job-ids context-jobs))
  (define (cancel jobs)
    (if (null? jobs) '()
        (cons (if (context-pending-job? (car jobs))
                  (hash-insert
                    (hash-insert (car jobs) 'status "cancelled")
                    'error reason)
                  (car jobs))
              (cancel (cdr jobs)))))
  (set! context-jobs (cancel context-jobs))
  ids)

(define (context-wait-result ids)
  (map (lambda (id) (context-job-public (context-job-find context-jobs id))) ids))

(define (context-queue-effect job request next)
  (hash 'type "queue_context_compaction"
        'job_id (hash-ref job 'id)
        'url (hash-ref request 'url)
        'secret (hash-ref request 'secret)
        'headers (hash-ref request 'headers)
        'body (hash-ref request 'body)
        'timeout_ms (hash-ref request 'timeout_ms)
        'stream (or (hash-try-get request 'stream) '())
        'next next))

(define (context-resume-effect activity messages usage model reasoning service-tier)
  (if (equal? activity "working")
      (request-effect messages usage model reasoning service-tier)
      (hash 'type "continue")))

(define (init encoded-config)
  (set! next-context-notification 25)
  (set! pending-context-calls '())
  (set! context-jobs '())
  (set! next-context-job 1)
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
  (set! pending-context-calls
        (or (hash-try-get state 'pending_context_calls) '()))
  (set! context-jobs (or (hash-try-get state 'context_jobs) '()))
  (set! next-context-job (or (hash-try-get state 'next_context_job) 1))
  (set! next-context-notification
        (or (hash-try-get state 'next_context_notification) 25))
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
  (define (append-context-result! call result)
    (set! messages
          (append messages (list (context-tool-result call result)))))
  (define (continue-context-calls)
    (cond
      [(null? pending-context-calls)
       (set! next-state
             (make-state messages compactions last-usage context-budget
                         model reasoning service-tier "working" "" 0))
       (request-effect messages last-usage model reasoning service-tier)]
      [(context-tool-name? (hash-ref (car pending-context-calls) 'name))
       (define call (car pending-context-calls))
       (define name (hash-ref call 'name))
       (set! pending-context-calls (cdr pending-context-calls))
       (set! messages (append messages (list call)))
       (set! active-context-items
             (context-sync-items active-context-items messages))
       (cond
         [(equal? name "context_inspect")
          (append-context-result!
            call
            (hash-insert
              (context-inspection active-context-items messages last-usage
                                  context-budget)
              'jobs (context-jobs-public context-jobs)))
          (continue-context-calls)]
         [(equal? name "context_mark")
          (define outcome
            (with-handler
              (lambda (error) (hash 'error (to-string error)))
              (let* ([arguments (provider-arguments-for model call)]
                     [label (hash-ref arguments 'label)]
                     [closed (hash-ref (context-last-item active-context-items) 'id)]
                     [opened (context-runtime-id "S" next-context-span)])
                (hash 'label label
                      'result (hash 'closed closed 'opened opened 'label label)))))
          (define error (hash-try-get outcome 'error))
          (if error
              (append-context-result! call (hash 'error error))
              (begin
                (append-context-result! call (hash-ref outcome 'result))
                (set! active-context-items
                      (context-sync-items active-context-items messages))
                (set! active-context-items
                      (context-mark-items
                        active-context-items
                        (context-runtime-id "S" next-context-span)
                        (hash-ref outcome 'label) (hash-ref call 'call_id)))
                (set! next-context-span (+ next-context-span 1))))
          (continue-context-calls)]
         [(equal? name "context_wait")
          (define outcome
            (with-handler
              (lambda (error) (hash 'error (to-string error)))
              (let* ([arguments (provider-arguments-for model call)]
                     [requested
                       (or (hash-try-get arguments 'job_ids)
                           (context-pending-job-ids context-jobs))])
                (define (unknown ids)
                  (cond [(null? ids) #f]
                        [(not (context-job-find context-jobs (car ids))) (car ids)]
                        [else (unknown (cdr ids))]))
                (define unknown-id (unknown requested))
                (if unknown-id
                    (hash 'error
                          (string-append "unknown context compaction job: " unknown-id))
                    (hash 'requested requested)))))
          (define error (hash-try-get outcome 'error))
          (if error
              (begin
                (append-context-result! call (hash 'error error))
                (continue-context-calls))
              (let ([requested (hash-ref outcome 'requested)])
                (define (collect-pending ids)
                  (cond
                    [(null? ids) '()]
                    [(context-pending-job?
                       (context-job-find context-jobs (car ids)))
                     (cons (car ids) (collect-pending (cdr ids)))]
                    [else (collect-pending (cdr ids))]))
                (if (null? (collect-pending requested))
                    (begin
                      (append-context-result!
                        call (hash 'jobs (context-wait-result requested)))
                      (continue-context-calls))
                    (begin
                      (set! next-state
                            (make-state messages compactions last-usage context-budget
                                        model reasoning service-tier
                                        "waiting_context" "" 0))
                      (hash 'type "wait_for_context_compactions"
                            'call_id (hash-ref call 'call_id)
                            'job_ids requested)))))]
         [else
          (define outcome
            (with-handler
              (lambda (error) (hash 'error (to-string error)))
              (let* ([arguments (provider-arguments-for model call)]
                     [ids (hash-ref arguments 'items)]
                     [validation
                       (context-validated-selection
                         active-context-items ids
                         (context-reserved-item-ids context-jobs))]
                     [error (hash-try-get validation 'error)])
                (if error
                    (hash 'error error)
                    (hash 'ids ids
                          'label (hash-ref arguments 'label)
                          'selected (hash-ref validation 'selected))))))
          (define error (hash-try-get outcome 'error))
          (if error
              (begin
                (append-context-result! call (hash 'error error))
                (continue-context-calls))
              (begin
                (define job-id (context-runtime-id "J" next-context-job))
                (define job
                  (hash 'id job-id 'items (hash-ref outcome 'ids)
                        'label (hash-ref outcome 'label)
                        'status "queued" 'error "" 'attempt 0
                        'snapshot (hash-ref outcome 'selected)))
                (set! next-context-job (+ next-context-job 1))
                (set! context-jobs (append context-jobs (list job)))
                (append-context-result!
                  call (hash 'job_id job-id 'status "queued"))
                (set! next-state
                      (make-state messages compactions last-usage context-budget
                                  model reasoning service-tier "working" "" 0))
                (context-queue-effect
                  job
                  (start-context-summary
                    (context-selected-messages (hash-ref outcome 'selected))
                    model reasoning service-tier)
                  (continue-context-calls))))])]
      [else
       (define calls
         (context-leading-ordinary-calls pending-context-calls))
       (set! pending-context-calls
             (context-after-leading-ordinary-calls pending-context-calls))
       (set! messages (append messages calls))
       (set! next-state
             (make-state messages compactions last-usage context-budget
                         model reasoning service-tier "working" "" 0))
       (hash 'type "run_tools"
             'calls (map (lambda (call) (tool-call-execution model call)) calls))]))
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
       (request-effect messages last-usage model reasoning service-tier)]
      [(equal? event-type "compact_requested")
       (if (null? messages)
           (begin
             (context-cancel-pending! "superseded by full compaction")
             (set! next-state
                   (make-state messages compactions last-usage context-budget
                               model reasoning service-tier "ready" "" 0))
             (hash 'type "finish" 'content "Nothing to compact."))
           (begin
             (context-cancel-pending! "superseded by full compaction")
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
       (reset-context-pressure-notification!
         messages last-usage context-budget)
       (set! next-state
             (make-state messages compactions last-usage
                         context-budget model reasoning service-tier "ready" "" 0))
       (hash 'type "finish" 'content
             (string-append "Model set to " model " " reasoning
                            " " service-tier))]
      [(equal? event-type "context_compaction_started")
       (define job-id (hash-ref event 'job_id))
       (define job (context-job-find context-jobs job-id))
       (if (and job (not (context-terminal-status? (hash-ref job 'status))))
           (context-job-update-status job-id "running" ""))
       (set! next-state
             (make-state messages compactions last-usage context-budget
                         model reasoning service-tier activity pending-finish
                         compaction-attempt))
       (hash 'type "continue")]
      [(equal? event-type "context_compaction_completed")
       (define job-id (hash-ref event 'job_id))
       (define job (context-job-find context-jobs job-id))
       (cond
         [(or (not job) (context-terminal-status? (hash-ref job 'status)))
          (set! next-state
                (make-state messages compactions last-usage context-budget
                            model reasoning service-tier activity pending-finish
                            compaction-attempt))
          (context-resume-effect activity messages last-usage model reasoning service-tier)]
         [(not (hash-ref event 'success))
          (context-job-update-status job-id "failed" (hash-ref event 'error))
          (set! next-state
                (make-state messages compactions last-usage context-budget
                            model reasoning service-tier activity pending-finish
                            compaction-attempt))
          (context-resume-effect activity messages last-usage model reasoning service-tier)]
         [else
          (define summary-text
            (complete-context-summary (hash-ref event 'events) model))
          (define attempt (or (hash-try-get job 'attempt) 0))
          (if (equal? summary-text "")
              (if (< attempt 4)
                  (begin
                    (define updated (hash-insert job 'attempt (+ attempt 1)))
                    (set! context-jobs
                          (context-job-replace context-jobs job-id updated))
                    (set! next-state
                          (make-state messages compactions last-usage context-budget
                                      model reasoning service-tier activity
                                      pending-finish compaction-attempt))
                    (context-queue-effect
                      updated
                      (start-context-summary-repair
                        (context-selected-messages (hash-ref job 'snapshot))
                        model reasoning service-tier)
                      (context-resume-effect
                        activity messages last-usage model reasoning service-tier)))
                  (begin
                    (context-job-update-status
                      job-id "failed"
                      "selective context compactor returned no summary after 4 repair attempts")
                    (set! next-state
                          (make-state messages compactions last-usage context-budget
                                      model reasoning service-tier activity
                                      pending-finish compaction-attempt))
                    (context-resume-effect
                      activity messages last-usage model reasoning service-tier)))
              (let* ([ids (hash-ref job 'items)]
                     [current
                       (context-compatible-selection
                         active-context-items ids (hash-ref job 'snapshot))])
                (if (not current)
                    (begin
                      (context-job-update-status
                        job-id "stale" "selected context changed before apply")
                      (set! next-state
                            (make-state messages compactions last-usage context-budget
                                        model reasoning service-tier activity
                                        pending-finish compaction-attempt))
                      (context-resume-effect
                        activity messages last-usage model reasoning service-tier))
                    (begin
                      (define summary-message
                        (hash 'kind "message" 'role "user"
                              'content (string-append "Context summary:\n" summary-text)))
                      (define summary
                        (context-summary-item
                          (context-runtime-id "C" next-context-summary)
                          (hash-ref job 'label) summary-message
                          (context-sum-from-tokens current) ids current
                          (context-selection-after current)))
                      (set! next-context-summary (+ next-context-summary 1))
                      (set! active-context-items
                            (context-replace-selection active-context-items ids summary))
                      (set! messages (context-flatten-items active-context-items))
                      (context-job-update-status job-id "applied" "")
                      (set! compactions (+ compactions 1))
                      (reset-context-pressure-notification!
                        messages last-usage context-budget)
                      (set! next-state
                            (make-state messages compactions last-usage context-budget
                                        model reasoning service-tier activity
                                        pending-finish compaction-attempt))
                      (context-resume-effect
                        activity messages last-usage model reasoning
                        service-tier)))))])]
      [(equal? event-type "context_wait_completed")
       (define ids (hash-ref event 'job_ids))
       (define call-id (hash-ref event 'call_id))
       (set! messages
             (append messages
                     (list
                       (hash 'kind "tool_result" 'call_id call-id
                             'content
                             (value->jsexpr-string
                               (hash 'jobs (context-wait-result ids)))))))
       (continue-context-calls)]
      [(equal? event-type "context_compactions_cancelled")
       (map (lambda (id)
              (define job (context-job-find context-jobs id))
              (if (and job (context-pending-job? job))
                  (context-job-update-status
                    id "cancelled" (hash-ref event 'reason))))
            (hash-ref event 'job_ids))
       (set! next-state
             (make-state messages compactions last-usage context-budget
                         model reasoning service-tier activity pending-finish
                         compaction-attempt))
       (hash 'type "continue")]
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
                (request-effect messages last-usage model reasoning service-tier))
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
                    (request-effect messages last-usage model reasoning service-tier)))
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
                                              context-budget)
                          'usage)))
                (set! messages
                      (append messages
                              (list (hash 'kind "tool_result"
                                          'call_id (hash-ref pending-context 'call_id)
                                          'content (value->jsexpr-string result)))))
                (set! pending-context (hash))
                (set! compactions (+ compactions 1))
                (reset-context-pressure-notification!
                  messages last-usage context-budget)
                (set! next-state
                      (make-state messages compactions last-usage context-budget
                                  model reasoning service-tier "working" "" 0))
                (request-effect messages last-usage model reasoning service-tier)))]
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
                (reset-context-pressure-notification!
                  messages last-usage context-budget)
                (if (equal? pending-finish "")
                    (begin
                      (set! next-state
                            (make-state messages compactions last-usage
                                        context-budget model reasoning service-tier
                                        "working" "" 0))
                      (request-effect messages last-usage model reasoning service-tier))
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
          (if usage
              (set! last-usage
                    (hash-insert usage '_message_tokens
                                 (estimated-message-tokens messages))))
          (set! messages (append messages preserved))
          (if (not (null? calls))
              (begin
                (set! pending-context-calls calls)
                (continue-context-calls))
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
                      (context-cancel-pending! "superseded by automatic full compaction")
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
       (if (not (null? pending-context-calls))
           (continue-context-calls)
           (if (selected-compaction-needed? messages last-usage context-budget)
               (begin
                 (context-cancel-pending! "superseded by automatic full compaction")
                 (set! next-state
                       (make-state messages compactions last-usage context-budget
                                   model reasoning service-tier "compacting" "" 0))
                 (start-selected-compaction messages context-budget))
               (begin
                 (set! next-state
                       (make-state messages compactions last-usage context-budget
                                   model reasoning service-tier "working" "" 0))
                 (request-effect messages last-usage model reasoning service-tier))))]
      [else (hash 'type "finish" 'content "Unsupported event.")]))
  (set! next-state
        (hash-insert next-state 'next_context_notification
                     next-context-notification))
  (value->jsexpr-string
    (hash 'state (value->jsexpr-string next-state)
          'effects (list effect))))

(define (request-effect messages usage model reasoning service-tier)
  (define tools (tools-for-model model))
  (define context-budget
    (hash-ref (model-spec model) 'compaction_token_limit))
  (define used (estimated-context-tokens messages usage))
  (define pressure
    (if (<= context-budget 0) 0 (quotient (* used 100) context-budget)))
  (define crossed
    (context-highest-crossed-threshold
      pressure next-context-notification context-pressure-thresholds #f))
  (define request-messages
    (if (and crossed (context-tool-available? tools "context_compact"))
        (begin
          (set! next-context-notification
                (context-next-threshold crossed context-pressure-thresholds))
          (append
            messages
            (list (context-pressure-notification crossed pressure used context-budget))))
        messages))
  (define prompt
    (build-selected-prompt request-messages agent-instructions tools))
  (define output-schema (runtime-config-value 'output_schema #f))
  (provider-request
    (if output-schema
        (hash-insert prompt 'output_schema output-schema)
        prompt)
    model reasoning service-tier))

(define (context-tool-available? tools name)
  (cond
    [(null? tools) #f]
    [(and (hash-try-get (car tools) 'name)
          (equal? (hash-ref (car tools) 'name) name)) #t]
    [else (context-tool-available? (cdr tools) name)]))

(define (context-highest-crossed-threshold percent next thresholds found)
  (cond
    [(null? thresholds) found]
    [(< (car thresholds) next)
     (context-highest-crossed-threshold percent next (cdr thresholds) found)]
    [(<= (car thresholds) percent)
     (context-highest-crossed-threshold
       percent next (cdr thresholds) (car thresholds))]
    [else found]))

(define (context-next-threshold percent thresholds)
  (cond
    [(null? thresholds) 101]
    [(> (car thresholds) percent) (car thresholds)]
    [else (context-next-threshold percent (cdr thresholds))]))

(define (context-reset-pressure-threshold messages usage context-budget)
  (define used (estimated-context-tokens messages usage))
  (define percent
    (if (<= context-budget 0) 0 (quotient (* used 100) context-budget)))
  (context-next-threshold percent context-pressure-thresholds))

(define (reset-context-pressure-notification! messages usage context-budget)
  (set! next-context-notification
        (context-reset-pressure-threshold messages usage context-budget)))

(define (context-pressure-notification threshold percent used limit)
  (hash
    'kind "message"
    'role "user"
    'content
    (string-append
      "Internal context-management notice: active context has crossed "
      (number->string threshold) "% of the usable window (approximately "
      (number->string percent) "%, " (number->string used) " of "
      (number->string limit) " tokens). "
      (cond
        [(= threshold 25)
         (string-append
           "Advisory housekeeping only: when it is easy and low-risk, consider "
           "using context_inspect and compacting one or more obviously completed "
           "closed spans. This is optional and should not interrupt the task.")]
        [(= threshold 50)
         (string-append
           "Context cleanup is encouraged soon: make a deliberate pass with "
           "context_inspect and compact substantial completed closed spans when "
           "that can be done safely. This is not critical and does not require "
           "immediate action.")]
        [else
         (string-append
           "Give high priority to reducing active context before undertaking "
           "substantial new work. Use context_inspect and compact eligible "
           "completed closed spans, potentially combining adjacent spans in one "
           "context_compact call. Do not compact the open item or fixed context, "
           "and continue without compaction if nothing safe is eligible.")]))))

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
        'pending_context_calls pending-context-calls
        'context_jobs context-jobs
        'next_context_job next-context-job
        'next_context_notification next-context-notification
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
