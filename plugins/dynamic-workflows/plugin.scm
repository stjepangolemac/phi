;; Dynamic JavaScript workflows. The runner remains plugin-owned while Phi
;; supplies background task lifecycle and one-shot JSON-RPC agent processes.

(define (workflow-tool)
  (hash
    'name "Workflow"
    'strict_mode "loose"
    'description
    (string-append
      "Launch a JavaScript workflow in the background. Name-only calls discover ~/.phi/workflows, .phi/workflows, then loaded plugins; path selects an exact file in one of those roots; source runs one-off JavaScript without creating a reusable definition. Use TaskOutput to wait for or inspect the task and TaskStop to cancel it."
      (runtime-config-value 'workflow_help ""))
    'parameters
    (hash 'type "object"
          'properties
          (hash 'name (hash 'type "string" 'description "Declared workflow name to run.")
                'path
                (hash 'type "string"
                      'description "Optional exact .js workflow path. Relative paths resolve from the workspace; absolute paths are also accepted within allowed workflow roots.")
                'source
                (hash 'type "string"
                      'description "One-off JavaScript workflow source. Mutually exclusive with name and path; meta.name supplies the task display name.")
                'args
                (hash 'description "JSON value passed to the workflow function. Declared input schemas are listed in the tool description."))
          'required (list "args")
          'oneOf
          (list
            (hash 'required (list "name")
                  'not (hash 'required (list "source")))
            (hash 'required (list "source")
                  'not
                  (hash 'anyOf
                        (list (hash 'required (list "name"))
                              (hash 'required (list "path"))))))
          'additionalProperties #f)))

(define (task-output-tool)
  (hash
    'name "TaskOutput"
    'description
    "Inspect a workflow task. Use null for the 15-second default, another wait in milliseconds, or 0 to inspect immediately."
    'parameters
    (hash 'type "object"
          'properties
          (hash 'task_id (hash 'type "string")
                'wait_ms
                (hash 'type (list "integer" "null") 'minimum 0 'maximum 300000
                      'description "Milliseconds to wait. Use null for 15000 or 0 to inspect immediately."))
          'required (list "task_id" "wait_ms")
          'additionalProperties #f)))

(define (task-stop-tool)
  (hash
    'name "TaskStop"
    'description "Cancel a running workflow task and its child Phi agents."
    'parameters
    (hash 'type "object"
          'properties (hash 'task_id (hash 'type "string"))
          'required (list "task_id")
          'additionalProperties #f)))

(register-tool! workflow-tool (hash 'mode "workflow" 'action "launch"))
(register-tool! task-output-tool (hash 'mode "workflow" 'action "output"))
(register-tool! task-stop-tool (hash 'mode "workflow" 'action "stop"))
