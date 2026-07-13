;; Keep the newest usable context and replace older turns with a small marker.
(define (compact-messages messages max-chars)
  (if (<= (encoded-length messages) max-chars)
      messages
      (let* ([minimum (if (and (> (length messages) 1)
                               (equal? (hash-ref (last-message messages) 'kind)
                                       "tool_result"))
                          2
                          1)]
             [recent-count (min 6 (length messages))]
             [recent (drop-first messages (- (length messages) recent-count))])
        (fit-recent recent minimum max-chars))))

(define (fit-recent recent minimum max-chars)
  (define candidate (cons (compaction-marker) recent))
  (cond
    [(<= (encoded-length candidate) max-chars) candidate]
    [(> (length recent) minimum)
     (fit-recent (cdr recent) minimum max-chars)]
    [else
     (fit-required recent max-chars (encoded-length recent))]))

;; A single message or tool call/result pair can itself exceed the budget.
(define (fit-required required max-chars content-limit)
  (define candidate
    (cons (compaction-marker)
          (map (lambda (message) (truncate-message message content-limit))
               required)))
  (if (or (<= (encoded-length candidate) max-chars)
          (= content-limit 0))
      candidate
      (fit-required required max-chars (quotient content-limit 2))))

(define (truncate-message message limit)
  (define kind (hash-ref message 'kind))
  (cond
    [(equal? kind "message")
     (hash 'kind kind
           'role (hash-ref message 'role)
           'content (truncate-text (hash-ref message 'content) limit))]
    [(equal? kind "tool_call")
     (hash 'kind kind
           'call_id (hash-ref message 'call_id)
           'name (hash-ref message 'name)
           'arguments (truncate-text (hash-ref message 'arguments) limit))]
    [(equal? kind "tool_result")
     (hash 'kind kind
           'call_id (hash-ref message 'call_id)
           'content (truncate-text (hash-ref message 'content) limit))]
    [else message]))

(define (truncate-text text limit)
  (if (> (string-length text) limit)
      (string-append (substring text 0 limit) "...[truncated]")
      text))

(define (compaction-marker)
  (hash 'kind "message" 'role "user"
        'content "Earlier context was compacted."))

(define (encoded-length messages)
  (string-length (value->jsexpr-string messages)))

(define (last-message messages)
  (if (null? (cdr messages))
      (car messages)
      (last-message (cdr messages))))

(define (drop-first items count)
  (if (or (= count 0) (null? items))
      items
      (drop-first (cdr items) (- count 1))))
