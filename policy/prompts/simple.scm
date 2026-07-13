;; Provider-neutral prompt assembly.
(define (build-prompt messages instructions tools)
  (hash 'instructions instructions
        'messages messages
        'tools tools))
