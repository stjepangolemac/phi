;; Agent-directed context boundaries, inspection, and selective compaction.

(define context-mark-spec
  (hash 'name "context_mark"
        'description "Close the current context span and begin a new labeled span. Use this for a meaningful change in focus; it does not require an active plan."
        'parameters
        (hash 'type "object"
              'properties (hash 'label (hash 'type "string" 'minLength 1))
              'required (list "label")
              'additionalProperties #f)))

(define context-inspect-spec
  (hash 'name "context_inspect"
        'description "Inspect context usage and the ordered active raw and summary items. Only closed items can be selected for compaction."
        'parameters
        (hash 'type "object" 'properties (hash)
              'additionalProperties #f)))

(define context-compact-spec
  (hash 'name "context_compact"
        'description "Compact one or more ordered, adjacent, closed context items into one summary. The fixed prompt and current open item are never selectable."
        'parameters
        (hash 'type "object"
              'properties
              (hash 'items
                    (hash 'type "array" 'minItems 1
                          'items (hash 'type "string"))
                    'label (hash 'type "string" 'minLength 1))
              'required (list "items" "label")
              'additionalProperties #f)))

(register-tool! (lambda () context-mark-spec))
(register-tool! (lambda () context-inspect-spec))
(register-tool! (lambda () context-compact-spec))
