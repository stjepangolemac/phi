;; Summarize oversized history through any registered model.
(define (compaction-needed? messages max-chars _config)
  (> (encoded-length messages) max-chars))

(define (start-compaction messages _max-chars config)
  (provider-request
    (hash 'instructions
          "Summarize the conversation for another model that will continue the work. Preserve user requirements, decisions, file paths, code changes, tool results, unresolved work, and current state. Be concise and return only the summary."
          'messages (portable-messages messages)
          'tools '())
    (hash-ref config 'model)
    (hash-ref config 'reasoning)
    (hash-ref config 'service_tier)))

(define (complete-compaction _messages max-chars events config)
  (define summary
    (provider-output-for (hash-ref config 'model) events))
  (if (equal? summary "")
      (error! "compactor returned no summary")
      (fit-summary summary max-chars)))

(define (portable-messages messages)
  (cond
    [(null? messages) '()]
    [(equal? (hash-ref (car messages) 'kind) "provider_item")
     (portable-messages (cdr messages))]
    [else (cons (car messages) (portable-messages (cdr messages)))]))

(define (fit-summary summary max-chars)
  (define candidate
    (list (hash 'kind "message" 'role "user"
                'content (string-append "Conversation summary:\n" summary))))
  (cond
    [(<= (encoded-length candidate) max-chars) candidate]
    [(equal? summary "") (error! "compaction budget is too small")]
    [else
     (fit-summary
       (substring summary 0 (quotient (string-length summary) 2))
       max-chars)]))

(define (encoded-length messages)
  (string-length (value->jsexpr-string messages)))

(register-compactor! "simple" compaction-needed? start-compaction
                     complete-compaction)
