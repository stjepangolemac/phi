;; Dynamic JavaScript workflows. The runner remains plugin-owned while Phi
;; supplies background task lifecycle and one-shot JSON-RPC agent processes.

(define (workflow-tool)
  (hash
    'name "Workflow"
    'strict_mode "loose"
    'description
    "Launch a named JavaScript workflow in the background. Workflows are discovered from .phi/workflows, ~/.phi/workflows, then loaded plugins. Use TaskOutput to wait for or inspect the task and TaskStop to cancel it."
    'parameters
    (hash 'type "object"
          'properties
          (hash 'name (hash 'type "string" 'description "Named workflow to run.")
                'args
                (hash 'type "object"
                      'description "JSON object passed to the workflow function."
                      'additionalProperties #t))
          'required (list "name" "args")
          'additionalProperties #f)))

(define (task-output-tool)
  (hash
    'name "TaskOutput"
    'description
    "Inspect a workflow task. Optionally wait for completion or until the timeout expires."
    'parameters
    (hash 'type "object"
          'properties
          (hash 'task_id (hash 'type "string")
                'wait_ms
                (hash 'type (list "integer" "null") 'minimum 0 'maximum 300000
                      'description "Milliseconds to wait. Use null or 0 to inspect immediately."))
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

(register-tool! workflow-tool)
(register-tool! task-output-tool)
(register-tool! task-stop-tool)
