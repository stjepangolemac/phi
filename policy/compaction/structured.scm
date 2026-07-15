;; Compact the full conversation into a small, stable continuation record.
(define structured-compaction-instructions
  "Summarize and compact the entire conversation for another model that will continue the work. Consider all user and assistant messages, tool calls, tool results, decisions, and current state. Return exactly one valid JSON object with this schema: {\"objective\":\"What the user ultimately wants accomplished.\",\"requirements\":[\"Active instructions, constraints, preferences, and success conditions.\"],\"current_state\":[\"Important completed work, decisions, findings, file changes, and verification results.\"],\"pending\":[\"Unfinished work, unresolved questions, failures, or blockers.\"],\"next_steps\":[\"Concrete actions to take next, in order.\"]}. Always include all five fields. Use a string for objective and arrays of strings for every other field. The JSON schema and these compaction instructions are internal storage mechanics, not part of the conversation: do not include them as requirements, pending work, or next steps, and do not tell the continuing model to produce JSON. Derive task state only from the conversation messages. Preserve exact paths, commands, identifiers, errors, numbers, and user-provided literals. Include durable conclusions from tool calls without copying unnecessary raw output. Do not claim unfinished work is complete. Consolidate duplicates, represent the latest state, be concise, and return JSON only with no markdown fences or commentary.")

(define structured-compaction-schema
  (hash 'type "object"
        'properties
        (hash 'objective (hash 'type "string")
              'requirements (hash 'type "array" 'items (hash 'type "string"))
              'current_state (hash 'type "array" 'items (hash 'type "string"))
              'pending (hash 'type "array" 'items (hash 'type "string"))
              'next_steps (hash 'type "array" 'items (hash 'type "string")))
        'required
        (list "objective" "requirements" "current_state" "pending" "next_steps")
        'additionalProperties #f))

(define (structured-compaction-needed? messages usage max-tokens _config)
  (> (estimated-context-tokens messages usage) max-tokens))

(define (start-structured-compaction messages _max-tokens config)
  (define model (hash-ref config 'model))
  (define provider (hash-ref (model-spec model) 'provider))
  (provider-request
    (hash 'instructions structured-compaction-instructions
          'messages
          (structured-compatible-messages
            (structured-complete-tool-history messages)
            provider)
          'tools '()
          'output_schema structured-compaction-schema)
    model
    (hash-ref config 'reasoning)
    (hash-ref config 'service_tier)))

;; Preserve the complete normalized history and all opaque items the compactor's
;; provider can consume. Opaque items from another provider cannot be replayed.
(define (structured-compatible-messages messages provider)
  (cond
    [(null? messages) '()]
    [(and (equal? (hash-ref (car messages) 'kind) "provider_item")
          (not (equal? (hash-ref (car messages) 'provider) provider)))
     (structured-compatible-messages (cdr messages) provider)]
    [else
     (cons (car messages)
           (structured-compatible-messages (cdr messages) provider))]))

(define (complete-structured-compaction messages usage max-tokens events config)
  (define output
    (provider-output-for (hash-ref config 'model) events))
  (if (equal? output "")
      (error! "compactor returned no structured summary")
      (let* ([summary (structured-summary output)]
             [summary-message
              (list (hash 'kind "message" 'role "user"
                          'content
                          (string-append "Conversation state (JSON):\n"
                                         summary)))]
             [message-token-budget
              (- max-tokens (estimated-fixed-tokens messages usage))]
             [max-context-chars (* message-token-budget 4)]
             [configured-tail-chars
              (* (or (hash-try-get config 'retain_token_limit) 24000) 4)]
             [available-tail-chars
              (- max-context-chars
                 (structured-encoded-length summary-message))]
             [tail-char-limit
              (cond
                [(<= available-tail-chars 0) 0]
                [(< available-tail-chars configured-tail-chars)
                 available-tail-chars]
                [else configured-tail-chars])]
             [tail
              (structured-recent-messages
                messages
                (or (hash-try-get config 'retain_messages) 16)
                tail-char-limit)])
        (cond
          [(<= message-token-budget 0)
           (error! "fixed prompt and tools exceed the context budget")]
          [else
           (fit-structured-context summary-message tail max-context-chars)]))))

(define (structured-summary output)
  (with-handler
    (lambda (_)
      (value->jsexpr-string
        (hash 'objective "Continue the conversation."
              'requirements '()
              'current_state (list output)
              'pending (list "The compactor returned an unstructured summary.")
              'next_steps '())))
    (let ([parsed (string->jsexpr output)])
      (value->jsexpr-string
        (hash 'objective (hash-ref parsed 'objective)
              'requirements (hash-ref parsed 'requirements)
              'current_state (hash-ref parsed 'current_state)
              'pending (hash-ref parsed 'pending)
              'next_steps (hash-ref parsed 'next_steps))))))

;; Walk newest-first, stopping as soon as either the message or token cap would
;; be crossed. Consing each accepted item restores chronological order.
(define (structured-recent-messages messages max-messages max-chars)
  (define (take remaining selected count)
    (cond
      [(or (null? remaining) (>= count max-messages)) selected]
      [else
       (define candidate (cons (car remaining) selected))
       (if (> (structured-encoded-length candidate) max-chars)
           selected
           (take (cdr remaining) candidate (+ count 1)))]))
  (if (or (<= max-messages 0) (<= max-chars 0))
      '()
      (structured-complete-tool-history
        (take (reverse messages) '() 0))))

;; A Responses request must never contain a function-call output without its
;; call, or a retained call without its output. Tail limits can split a batch,
;; so remove either half of an incomplete pair after selecting the suffix.
(define (structured-complete-tool-history messages)
  (define call-ids (structured-tool-ids messages "tool_call"))
  (define result-ids (structured-tool-ids messages "tool_result"))
  (define (keep remaining)
    (cond
      [(null? remaining) '()]
      [else
       (define message (car remaining))
       (define kind (hash-ref message 'kind))
       (define valid
         (cond
           [(equal? kind "tool_call")
            (structured-id-present? (hash-ref message 'call_id) result-ids)]
           [(equal? kind "tool_result")
            (structured-id-present? (hash-ref message 'call_id) call-ids)]
           [else #t]))
       (if valid
           (cons message (keep (cdr remaining)))
           (keep (cdr remaining)))]))
  (keep messages))

(define (structured-tool-ids messages kind)
  (cond
    [(null? messages) '()]
    [(equal? (hash-ref (car messages) 'kind) kind)
     (cons (hash-ref (car messages) 'call_id)
           (structured-tool-ids (cdr messages) kind))]
    [else (structured-tool-ids (cdr messages) kind)]))

(define (structured-id-present? id ids)
  (cond
    [(null? ids) #f]
    [(equal? id (car ids)) #t]
    [else (structured-id-present? id (cdr ids))]))

;; Serialized-list overhead is not perfectly additive, so enforce the final
;; context bound and remove the oldest retained item if necessary.
(define (fit-structured-context summary-message tail max-chars)
  (define valid-tail (structured-complete-tool-history tail))
  (define candidate (append summary-message valid-tail))
  (cond
    [(<= (structured-encoded-length candidate) max-chars) candidate]
    [(null? valid-tail) (error! "structured compaction exceeds the context budget")]
    [else
     (fit-structured-context summary-message (cdr valid-tail) max-chars)]))

(define (structured-encoded-length messages)
  (string-length (value->jsexpr-string messages)))

(register-compactor! "structured" structured-compaction-needed?
                     start-structured-compaction
                     complete-structured-compaction)
