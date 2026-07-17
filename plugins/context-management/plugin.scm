;; Agent-directed context boundaries, inspection, and selective compaction.

(define context-mark-spec
  (hash 'name "context_mark"
        'description "Close the current context span and begin a new labeled span. Call this proactively after completing a substantial phase or when changing focus so finished work becomes eligible for selective compaction; no active plan is required."
        'parameters
        (hash 'type "object"
              'properties (hash 'label (hash 'type "string" 'minLength 1))
              'required (list "label")
              'additionalProperties #f)))

(define context-inspect-spec
  (hash 'name "context_inspect"
        'description "Inspect context pressure and the ordered active raw and summary items. Use this when context pressure rises to identify substantial closed items worth compacting."
        'parameters
        (hash 'type "object" 'properties (hash)
              'additionalProperties #f)))

(define context-compact-spec
  (hash 'name "context_compact"
        'description "Compact one or more ordered, adjacent, closed context items into one durable summary. Use this proactively under context pressure when older completed work can be summarized without losing details needed for the current task. The fixed prompt and open item are never selectable."
        'parameters
        (hash 'type "object"
              'properties
              (hash 'items
                    (hash 'type "array" 'minItems 1
                          'items (hash 'type "string"))
                    'label (hash 'type "string" 'minLength 1))
              'required (list "items" "label")
              'additionalProperties #f)))

(define context-wait-spec
  (hash 'name "context_wait"
        'description "Wait for selected context-compaction jobs, or for all jobs pending when the call begins. Completed jobs return immediately."
        'parameters
        (hash 'type "object"
              'properties
              (hash 'job_ids
                    (hash 'type "array"
                          'items (hash 'type "string")))
              'additionalProperties #f)))

(register-tool! (lambda () context-mark-spec))
(register-tool! (lambda () context-inspect-spec))
(register-tool! (lambda () context-compact-spec))
(register-tool! (lambda () context-wait-spec))
