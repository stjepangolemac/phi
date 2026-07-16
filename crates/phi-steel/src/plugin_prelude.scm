
(require-builtin steel/json)
(require-builtin steel/hash)

(define command-registry '())
(define skill-registry '())
(define model-registry '())
(define tool-registry '())
(define plugin-tool-registry '())
(define tool-implementation-registry '())
(define tool-selection-registry '())
(define tool-config-registry '())
(define provider-registry '())
(define prompt-builder-registry '())
(define compactor-registry '())
(define file-editor-registry '())
(define selected-prompt-builder "")
(define selected-compactor "")
(define selected-compactor-config (hash))
(define selected-file-editor "")
(define agent-instructions "")
(define session-id "")
(define runtime-config (hash))
(define current-plugin "")

(define (configure-runtime! encoded-config)
  (define config (string->jsexpr encoded-config))
  (set! runtime-config config)
  (set! tool-registry (or (hash-try-get config 'tools) '()))
  (set! session-id (or (hash-try-get config 'session_id) "")))

(define (set-agent-instructions! value)
  (set! agent-instructions value))

(define (register-command! spec handler)
  (set! command-registry
        (append command-registry (list (hash 'spec spec 'handler handler)))))

(define (set-current-plugin! name) (set! current-plugin name))

(define (register-skill! spec)
  (if (equal? current-plugin "") (error! "skills must be registered by a plugin"))
  (set! skill-registry
        (append skill-registry
                (list (hash-insert spec 'plugin current-plugin)))))

(define (register-tool! builder)
  (set! plugin-tool-registry (append plugin-tool-registry (list builder))))

(define (register-provider! name effect call arguments output usage preserved phase)
  (set! provider-registry
        (append provider-registry
                (list (hash 'name name 'effect effect 'call call
                            'arguments arguments 'output output 'usage usage
                            'preserved preserved 'phase phase)))))

(define (remove-model-by-id models id)
  (cond [(null? models) '()]
        [(equal? id (hash-ref (car models) 'id))
         (remove-model-by-id (cdr models) id)]
        [else (cons (car models) (remove-model-by-id (cdr models) id))]))

(define (register-model! provider spec)
  (define model (hash-ref spec 'id))
  (define id (string-append provider "/" model))
  (set! model-registry
        (append (remove-model-by-id model-registry id)
                (list (hash-insert
                        (hash-insert
                          (hash-insert spec 'provider provider)
                          'model model)
                        'id id)))))

(define (unregister-model! id)
  (set! model-registry (remove-model-by-id model-registry id)))

(define (register-hosted-tool! name capability provider build)
  (set! tool-implementation-registry
        (append tool-implementation-registry
                (list (hash 'name name 'capability capability 'mode "hosted"
                            'provider provider 'build build)))))

(define (register-callable-tool! name capability parallel spec start complete)
  (set! tool-implementation-registry
        (append tool-implementation-registry
                (list (hash 'name name 'capability capability 'mode "callable"
                            'parallel parallel 'spec spec
                            'start start 'complete complete)))))

(define (configure-tool! name config)
  (set! tool-config-registry
        (append tool-config-registry (list (hash 'name name 'config config)))))

(define (select-tool! capability preferences)
  (set! tool-selection-registry
        (append tool-selection-registry
                (list (hash 'name capability 'preferences preferences)))))

(define (register-prompt-builder! name builder)
  (set! prompt-builder-registry
        (append prompt-builder-registry (list (hash 'name name 'builder builder)))))

(define (register-compactor! name needed start complete)
  (set! compactor-registry
        (append compactor-registry
                (list (hash 'name name 'needed needed 'start start
                            'complete complete)))))

(define (register-file-editor! name spec prepare propose)
  (set! file-editor-registry
        (append file-editor-registry
                (list (hash 'name name 'spec spec 'prepare prepare
                            'propose propose)))))

(define (select-prompt-builder! name) (set! selected-prompt-builder name))
(define (select-compactor! name config)
  (set! selected-compactor name)
  (set! selected-compactor-config config))
(define (select-file-editor! name) (set! selected-file-editor name))

(define (composition-status)
  (hash 'prompt_builder selected-prompt-builder
        'file_editor selected-file-editor
        'compactor selected-compactor))
(define (load-plugin! _) #t)

(define (registered-command-specs)
  (map (lambda (entry) (hash-ref entry 'spec)) command-registry))

(define (registered-skills) skill-registry)

(define (registered-models) model-registry)
(define (runtime-config-value name fallback)
  (or (hash-try-get runtime-config name) fallback))

(define (built-plugin-tools builders)
  (cond [(null? builders) '()]
        [else
         (define tool ((car builders)))
         (if tool
             (cons tool (built-plugin-tools (cdr builders)))
             (built-plugin-tools (cdr builders)))]))

(define (registered-tools)
  (append tool-registry (built-plugin-tools plugin-tool-registry)))
(define (runtime-session-id) session-id)

(define (find-named entries name)
  (cond [(null? entries) (error! (string-append "component not found: " name))]
        [(equal? name (hash-ref (car entries) 'name)) (car entries)]
        [else (find-named (cdr entries) name)]))

(define (model-spec id)
  (define (find models)
    (cond [(null? models) (error! (string-append "model not found: " id))]
          [(equal? id (hash-ref (car models) 'id)) (car models)]
          [else (find (cdr models))]))
  (find model-registry))

(define (model-provider id)
  (find-named provider-registry (hash-ref (model-spec id) 'provider)))

(define (string-member? value values)
  (cond [(null? values) #f]
        [(equal? value (car values)) #t]
        [else (string-member? value (cdr values))]))

(define (tool-config name)
  (define (find entries)
    (cond [(null? entries) (hash)]
          [(equal? name (hash-ref (car entries) 'name))
           (hash-ref (car entries) 'config)]
          [else (find (cdr entries))]))
  (find tool-config-registry))

(define (tool-compatible? implementation model)
  (define spec (model-spec model))
  (cond
    [(equal? (hash-ref implementation 'mode) "hosted")
     (and (equal? (hash-ref implementation 'provider)
                  (hash-ref spec 'provider))
          (string-member?
            (hash-ref implementation 'name)
            (or (hash-try-get spec 'hosted_tools) '())))]
    [(equal? (hash-ref implementation 'mode) "callable")
     (or (hash-try-get spec 'function_tools) #f)]
    [else #f]))

(define (find-compatible-hosted capability model implementations)
  (cond
    [(null? implementations) #f]
    [(and (equal? capability (hash-ref (car implementations) 'capability))
          (equal? (hash-ref (car implementations) 'mode) "hosted")
          (tool-compatible? (car implementations) model))
     (car implementations)]
    [else (find-compatible-hosted capability model (cdr implementations))]))

(define (resolve-tool-preference capability model preference)
  (define preferred (hash-try-get preference 'prefer))
  (define selected (hash-try-get preference 'use))
  (cond
    [(and preferred (equal? preferred "same-route-hosted"))
     (find-compatible-hosted capability model tool-implementation-registry)]
    [selected
     (define implementation
       (find-named tool-implementation-registry selected))
     (if (and (equal? capability (hash-ref implementation 'capability))
              (tool-compatible? implementation model))
         implementation
         #f)]
    [else (error! "invalid tool preference")]))

(define (resolve-tool-selection selection model)
  (define capability (hash-ref selection 'name))
  (define (resolve preferences)
    (cond
      [(null? preferences) #f]
      [else
       (define implementation
         (resolve-tool-preference capability model (car preferences)))
       (if implementation implementation (resolve (cdr preferences)))]))
  (resolve (hash-ref selection 'preferences)))

(define (resolved-tool-implementations model)
  (define (resolve selections)
    (cond
      [(null? selections) '()]
      [else
       (define implementation (resolve-tool-selection (car selections) model))
       (if implementation
           (cons implementation (resolve (cdr selections)))
           (resolve (cdr selections)))]))
  (resolve tool-selection-registry))

(define (resolved-tool-names model)
  (map (lambda (implementation) (hash-ref implementation 'name))
       (resolved-tool-implementations model)))

(define (resolved-tool-routes model)
  (map (lambda (implementation)
         (hash 'capability (hash-ref implementation 'capability)
               'implementation (hash-ref implementation 'name)))
       (resolved-tool-implementations model)))

(define (resolved-tool-spec implementation)
  (if (equal? (hash-ref implementation 'mode) "hosted")
      (hash 'kind "hosted_tool"
            'provider (hash-ref implementation 'provider)
            'implementation (hash-ref implementation 'name)
            'wire ((hash-ref implementation 'build)
                   (tool-config (hash-ref implementation 'name))))
      (hash-ref implementation 'spec)))

(define (tools-for-model model)
  (append (registered-tools)
          (if (equal? selected-file-editor "")
              '()
              (list (hash-ref
                      (find-named file-editor-registry selected-file-editor)
                      'spec)))
          (map resolved-tool-spec (resolved-tool-implementations model))))

(define (selected-file-editor-entry)
  (find-named file-editor-registry selected-file-editor))

(define (selected-file-editor-tool-name)
  (if (equal? selected-file-editor "")
      ""
      (hash-ref (hash-ref (selected-file-editor-entry) 'spec) 'name)))

(define (prepare-file-edit name arguments)
  (if (not (equal? name (selected-file-editor-tool-name)))
      (error! (string-append "unknown file editor tool: " name)))
  ((hash-ref (selected-file-editor-entry) 'prepare) arguments))

(define (propose-file-edit name plan snapshots)
  (if (not (equal? name (selected-file-editor-tool-name)))
      (error! (string-append "unknown file editor tool: " name)))
  ((hash-ref (selected-file-editor-entry) 'propose) plan snapshots))

(define (callable-tool-for model name)
  (define (find implementations)
    (cond
      [(null? implementations) #f]
      [(and (equal? (hash-ref (car implementations) 'mode) "callable")
            (equal? (hash-ref (hash-ref (car implementations) 'spec) 'name)
                    name))
       (car implementations)]
      [else (find (cdr implementations))]))
  (find (resolved-tool-implementations model)))

(define (start-callable-tool implementation arguments)
  ((hash-ref implementation 'start)
   arguments (tool-config (hash-ref implementation 'name))))

(define (complete-callable-tool implementation events)
  ((hash-ref implementation 'complete)
   events (tool-config (hash-ref implementation 'name))))

(define (provider-request prompt model reasoning service-tier)
  (define output-schema (hash-try-get prompt 'output_schema))
  (if (and output-schema
           (not (hash-try-get (model-spec model)
                              'strict_json_schema_capable)))
      (error! (string-append
                "model does not support strict JSON schema: " model)))
  ((hash-ref (model-provider model) 'effect)
   prompt (hash-ref (model-spec model) 'model) reasoning service-tier))
(define (provider-calls-for model events)
  ((hash-ref (model-provider model) 'call) events))
(define (provider-arguments-for model call)
  ((hash-ref (model-provider model) 'arguments) call))
(define (provider-output-for model events)
  ((hash-ref (model-provider model) 'output) events))
(define (provider-usage-for model events)
  ((hash-ref (model-provider model) 'usage) events))
(define (provider-preserved-items-for model events)
  ((hash-ref (model-provider model) 'preserved) events))
(define (provider-message-phase-for model events)
  ((hash-ref (model-provider model) 'phase) events))

(define (build-selected-prompt messages instructions tools)
  ((hash-ref (find-named prompt-builder-registry selected-prompt-builder) 'builder)
   messages instructions tools))

(define (estimated-message-tokens messages)
  (quotient (string-length (value->jsexpr-string messages)) 4))

(define (estimated-fixed-tokens messages usage)
  (define total (hash-try-get usage 'total_tokens))
  (define baseline (hash-try-get usage '_message_tokens))
  (if (and total baseline (> total baseline)) (- total baseline) 0))

(define (estimated-context-tokens messages usage)
  (define total (hash-try-get usage 'total_tokens))
  (define baseline (hash-try-get usage '_message_tokens))
  (if (and total (not baseline))
      total
      (+ (estimated-fixed-tokens messages usage)
         (estimated-message-tokens messages))))

(define (selected-compaction-needed? messages usage max-tokens)
  ((hash-ref (find-named compactor-registry selected-compactor) 'needed)
   messages usage max-tokens selected-compactor-config))

(define (start-selected-compaction messages max-tokens)
  ((hash-ref (find-named compactor-registry selected-compactor) 'start)
   messages max-tokens selected-compactor-config))

(define (complete-selected-compaction messages usage max-tokens events repair-count)
  ((hash-ref (find-named compactor-registry selected-compactor) 'complete)
   messages usage max-tokens events repair-count selected-compactor-config))

(define (validate-tool-preference preference)
  (define preferred (hash-try-get preference 'prefer))
  (define selected (hash-try-get preference 'use))
  (cond
    [(and preferred (equal? preferred "same-route-hosted")) #t]
    [selected (find-named tool-implementation-registry selected) #t]
    [else (error! "invalid tool preference")]))

(define (validate-composition!)
  (if (equal? selected-prompt-builder "") (error! "no prompt builder selected"))
  (if (equal? selected-compactor "") (error! "no compactor selected"))
  (find-named prompt-builder-registry selected-prompt-builder)
  (find-named compactor-registry selected-compactor)
  (if (not (equal? selected-file-editor ""))
      (find-named file-editor-registry selected-file-editor))
  (map (lambda (entry)
         (find-named tool-implementation-registry (hash-ref entry 'name)))
       tool-config-registry)
  (map (lambda (selection)
         (map validate-tool-preference (hash-ref selection 'preferences)))
       tool-selection-registry)
  #t)

(define (dispatch-command name state arguments)
  (define (find entries)
    (cond [(null? entries) (error! "unknown plugin command")]
          [(equal? name (hash-ref (hash-ref (car entries) 'spec) 'name))
           ((hash-ref (car entries) 'handler) state arguments)]
          [else (find (cdr entries))]))
  (find command-registry))
