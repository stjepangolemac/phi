;; Codex-style patch parsing and matching. Filesystem effects remain in Rust.

(define begin-patch "*** Begin Patch")
(define end-patch "*** End Patch")
(define add-file "*** Add File: ")
(define delete-file "*** Delete File: ")
(define update-file "*** Update File: ")
(define move-to "*** Move to: ")
(define end-of-file "*** End of File")

(define (drop-last values)
  (cond [(null? values) '()]
        [(null? (cdr values)) '()]
        [else (cons (car values) (drop-last (cdr values)))]))

(define (take values count)
  (if (or (= count 0) (null? values))
      '()
      (cons (car values) (take (cdr values) (- count 1)))))

(define (drop values count)
  (if (or (= count 0) (null? values))
      values
      (drop (cdr values) (- count 1))))

(define (join-lines lines)
  (cond [(null? lines) ""]
        [(null? (cdr lines)) (car lines)]
        [else (string-append (car lines) "\n" (join-lines (cdr lines)))]))

(define (path-after line prefix)
  (define path (trim (substring line (string-length prefix) (string-length line))))
  (if (equal? path "") (error! "patch file path is empty"))
  path)

(define (operation-header? line)
  (or (starts-with? line add-file)
      (starts-with? line delete-file)
      (starts-with? line update-file)
      (equal? line end-patch)))

(define (parse-add-lines lines content)
  (cond
    [(null? lines) (error! "patch is missing its end marker")]
    [(operation-header? (car lines))
     (hash 'content (reverse content) 'rest lines)]
    [(starts-with? (car lines) "+")
     (parse-add-lines
       (cdr lines)
       (cons (substring (car lines) 1 (string-length (car lines))) content))]
    [else (error! "added file lines must start with +")]))

(define (parse-hunk-lines lines old new changed end?)
  (cond
    [(null? lines)
     (hash 'old (reverse old) 'new (reverse new) 'changed changed
           'end_of_file end? 'rest lines)]
    [(or (operation-header? (car lines)) (starts-with? (car lines) "@@"))
     (hash 'old (reverse old) 'new (reverse new) 'changed changed
           'end_of_file end? 'rest lines)]
    [(equal? (car lines) end-of-file)
     (if end? (error! "duplicate end-of-file marker"))
     (if (and (not (null? (cdr lines)))
              (not (operation-header? (car (cdr lines))))
              (not (starts-with? (car (cdr lines)) "@@")))
         (error! "end-of-file marker must end a hunk"))
     (hash 'old (reverse old) 'new (reverse new) 'changed changed
           'end_of_file #t 'rest (cdr lines))]
    [(equal? (car lines) "") (error! "hunk lines require a prefix")]
    [(starts-with? (car lines) " ")
     (define value (substring (car lines) 1 (string-length (car lines))))
     (parse-hunk-lines (cdr lines) (cons value old) (cons value new)
                       changed end?)]
    [(starts-with? (car lines) "-")
     (parse-hunk-lines
       (cdr lines)
       (cons (substring (car lines) 1 (string-length (car lines))) old)
       new #t end?)]
    [(starts-with? (car lines) "+")
     (parse-hunk-lines
       (cdr lines) old
       (cons (substring (car lines) 1 (string-length (car lines))) new)
       #t end?)]
    [else (error! "invalid patch hunk line")]))

(define (parse-anchors lines anchors)
  (if (and (not (null? lines)) (starts-with? (car lines) "@@"))
      (parse-anchors
        (cdr lines)
        (cons (trim (substring (car lines) 2 (string-length (car lines))))
              anchors))
      (hash 'anchors (reverse anchors) 'rest lines)))

(define (any-changed-hunk? hunks)
  (cond [(null? hunks) #f]
        [(hash-ref (car hunks) 'changed) #t]
        [else (any-changed-hunk? (cdr hunks))]))

(define (parse-hunks lines hunks path)
  (cond
    [(null? lines) (error! "patch is missing its end marker")]
    [(operation-header? (car lines))
     (if (null? hunks) (error! "updated file requires a hunk"))
     (if (not (any-changed-hunk? hunks))
         (error! (string-append path ": patch makes no change")))
     (hash 'hunks (reverse hunks) 'rest lines)]
    [(not (starts-with? (car lines) "@@"))
     (error! "updated file requires a @@ hunk header")]
    [else
     (define parsed-anchors (parse-anchors lines '()))
     (define parsed
       (parse-hunk-lines (hash-ref parsed-anchors 'rest) '() '() #f #f))
     (parse-hunks
       (hash-ref parsed 'rest)
       (cons (hash 'anchors (hash-ref parsed-anchors 'anchors)
                   'old (hash-ref parsed 'old)
                   'new (hash-ref parsed 'new)
                   'changed (hash-ref parsed 'changed)
                   'end_of_file (hash-ref parsed 'end_of_file))
             hunks)
       path)]))

(define (parse-operations lines operations)
  (cond
    [(null? lines) (error! "patch is missing its end marker")]
    [(equal? (car lines) end-patch)
     (if (null? operations) (error! "patch contains no file operations"))
     (if (and (not (null? (cdr lines)))
              (not (and (null? (cdr (cdr lines)))
                        (equal? (car (cdr lines)) ""))))
         (error! "unexpected content after patch end"))
     (reverse operations)]
    [(starts-with? (car lines) add-file)
     (define path (path-after (car lines) add-file))
     (define parsed (parse-add-lines (cdr lines) '()))
     (parse-operations
       (hash-ref parsed 'rest)
       (cons (hash 'operation "add" 'path path
                   'lines (hash-ref parsed 'content))
             operations))]
    [(starts-with? (car lines) delete-file)
     (parse-operations
       (cdr lines)
       (cons (hash 'operation "delete"
                   'path (path-after (car lines) delete-file))
             operations))]
    [(starts-with? (car lines) update-file)
     (define path (path-after (car lines) update-file))
     (define remaining (cdr lines))
     (define destination "")
     (if (and (not (null? remaining)) (starts-with? (car remaining) move-to))
         (begin
           (set! destination (path-after (car remaining) move-to))
           (set! remaining (cdr remaining))))
     (define parsed (parse-hunks remaining '() path))
     (parse-operations
       (hash-ref parsed 'rest)
       (cons (hash 'operation "update" 'path path
                   'destination destination
                   'hunk_groups (list (hash-ref parsed 'hunks)))
             operations))]
    [else (error! "expected an add, delete, or update file header")]))

(define (parse-patch text)
  (define lines (split-many text "\n"))
  (if (or (null? lines) (not (equal? (car lines) begin-patch)))
      (error! "patch must start with *** Begin Patch"))
  (parse-operations (cdr lines) '()))

(define (plain-update-for? operation path)
  (and (equal? (hash-ref operation 'operation) "update")
       (equal? (hash-ref operation 'path) path)
       (equal? (hash-ref operation 'destination) "")))

(define (collect-update-groups operations path groups kept)
  (cond
    [(null? operations)
     (hash 'groups groups 'rest (reverse kept))]
    [(plain-update-for? (car operations) path)
     (collect-update-groups
       (cdr operations) path
       (append groups (hash-ref (car operations) 'hunk_groups)) kept)]
    [else
     (collect-update-groups
       (cdr operations) path groups (cons (car operations) kept))]))

;; Multiple plain update sections for one file are one atomic edit. Keep their
;; hunk groups separate so each section starts matching from the file beginning.
(define (normalize-operations operations)
  (cond
    [(null? operations) '()]
    [(and (equal? (hash-ref (car operations) 'operation) "update")
          (equal? (hash-ref (car operations) 'destination) ""))
     (define operation (car operations))
     (define collected
       (collect-update-groups
         (cdr operations) (hash-ref operation 'path)
         (hash-ref operation 'hunk_groups) '()))
     (cons
       (hash 'operation "update"
             'path (hash-ref operation 'path)
             'destination ""
             'hunk_groups (hash-ref collected 'groups))
       (normalize-operations (hash-ref collected 'rest)))]
    [else
     (cons (car operations) (normalize-operations (cdr operations)))]))

(define (targets-for operations)
  (cond
    [(null? operations) '()]
    [else
     (define operation (car operations))
     (define destination (or (hash-try-get operation 'destination) ""))
     (append
       (list (hash 'path (hash-ref operation 'path)))
       (if (equal? destination "") '() (list (hash 'path destination)))
       (targets-for (cdr operations)))]))

(define (prepare-patch arguments)
  (define text (hash-ref arguments 'patch))
  (define operations (normalize-operations (parse-patch text)))
  (hash 'plan operations 'targets (targets-for operations)))

(define (snapshot-for snapshots path)
  (cond [(null? snapshots) (error! (string-append "missing snapshot: " path))]
        [(equal? path (hash-ref (car snapshots) 'path)) (car snapshots)]
        [else (snapshot-for (cdr snapshots) path)]))

(define (sequence-at? lines sequence index)
  (cond [(null? sequence) #t]
        [(>= index (length lines)) #f]
        [(equal? (list-ref lines index) (car sequence))
         (sequence-at? lines (cdr sequence) (+ index 1))]
        [else #f]))

(define (find-sequence lines sequence index)
  (cond [(null? sequence) index]
        [(> (+ index (length sequence)) (length lines)) #f]
        [(sequence-at? lines sequence index) index]
        [else (find-sequence lines sequence (+ index 1))]))

(define (find-line lines value index)
  (cond [(>= index (length lines)) #f]
        [(equal? (list-ref lines index) value) index]
        [else (find-line lines value (+ index 1))]))

(define (file-lines content)
  (cond [(equal? content "") (hash 'lines '() 'newline #f)]
        [(ends-with? content "\n")
         (hash 'lines (drop-last (split-many content "\n")) 'newline #t)]
        [else (hash 'lines (split-many content "\n") 'newline #f)]))

(define (render-lines lines newline?)
  (define content (join-lines lines))
  (if newline? (string-append content "\n") content))

(define (hunk-error path index message)
  (error! (string-append path ": hunk " (to-string index) " " message)))

(define (after-anchors lines anchors cursor path hunk-index)
  (cond
    [(null? anchors) cursor]
    [(equal? (car anchors) "")
     (after-anchors lines (cdr anchors) cursor path hunk-index)]
    [else
     (define index (find-line lines (car anchors) cursor))
     (if (equal? index #f)
         (hunk-error path hunk-index
                     (string-append "anchor not found: " (car anchors))))
     (after-anchors lines (cdr anchors) (+ index 1) path hunk-index)]))

(define (apply-hunk lines cursor hunk path hunk-index)
  (define anchors (hash-ref hunk 'anchors))
  (define old (hash-ref hunk 'old))
  (define new (hash-ref hunk 'new))
  (define end? (hash-ref hunk 'end_of_file))
  (define start (after-anchors lines anchors cursor path hunk-index))
  (define index
    (if (null? old)
        (if end? (length lines) start)
        (find-sequence lines old start)))
  (if (equal? index #f) (hunk-error path hunk-index "context not found"))
  (if (and (null? anchors)
           (not (null? old))
           (not (equal? (find-sequence lines old (+ index 1)) #f)))
      (hunk-error path hunk-index "context is ambiguous"))
  (if (and end? (not (= (+ index (length old)) (length lines))))
      (hunk-error path hunk-index "does not end at end of file"))
  (hash 'lines
        (append (take lines index) new
                (drop lines (+ index (length old))))
        'cursor (+ index (length new))))

(define (apply-hunks lines cursor hunks path hunk-index)
  (if (null? hunks)
      lines
      (let ([result (apply-hunk lines cursor (car hunks) path hunk-index)])
        (apply-hunks (hash-ref result 'lines) (hash-ref result 'cursor)
                     (cdr hunks) path (+ hunk-index 1)))))

(define (apply-hunk-groups lines groups path)
  (if (null? groups)
      lines
      (apply-hunk-groups
        (apply-hunks lines 0 (car groups) path 1)
        (cdr groups) path)))

(define (propose-operation operation snapshots)
  (define kind (hash-ref operation 'operation))
  (define path (hash-ref operation 'path))
  (define snapshot (snapshot-for snapshots path))
  (cond
    [(equal? kind "add")
     (if (hash-ref snapshot 'exists) (error! "added file already exists"))
     (define lines (hash-ref operation 'lines))
     (hash 'operation "create" 'path path
           'content (if (null? lines) "" (string-append (join-lines lines) "\n")))]
    [(equal? kind "delete")
     (if (not (hash-ref snapshot 'exists)) (error! "deleted file does not exist"))
     (hash 'operation "delete" 'path path)]
    [(equal? kind "update")
     (if (not (hash-ref snapshot 'exists)) (error! "updated file does not exist"))
     (define destination (hash-ref operation 'destination))
     (define original (file-lines (hash-ref snapshot 'content)))
     (define content
       (render-lines
         (apply-hunk-groups
           (hash-ref original 'lines)
           (hash-ref operation 'hunk_groups) path)
         (hash-ref original 'newline)))
     (if (and (equal? destination "")
              (equal? content (hash-ref snapshot 'content)))
         (error! (string-append path ": patch makes no change")))
     (if (equal? destination "")
         (hash 'operation "replace" 'path path 'content content)
         (begin
           (if (hash-ref (snapshot-for snapshots destination) 'exists)
               (error! "move destination already exists"))
           (hash 'operation "move" 'path path 'destination destination
                 'content content)))]
    [else (error! "unsupported parsed file operation")]))

(define (propose-operations operations snapshots)
  (if (null? operations)
      '()
      (cons (propose-operation (car operations) snapshots)
            (propose-operations (cdr operations) snapshots))))

(define (propose-patch plan snapshots)
  (propose-operations plan snapshots))

(register-file-editor!
  "codex-patch"
  (hash
    'name "patch"
    'description
    "Apply a Codex-style workspace patch. Wrap operations in *** Begin Patch and *** End Patch. Use *** Add File: path with every content line prefixed +; *** Delete File: path; or *** Update File: path, optional *** Move to: path, and @@ contextual hunks whose lines begin with space, -, or +. Put locator text on the @@ line, or use a context-only hunk before a later changing hunk. Repeated plain update sections for one file are applied sequentially as one atomic edit. Every update must change file content or destination. Read files before updating them."
    'parameters
    (hash 'type "object"
          'properties (hash 'patch (hash 'type "string"))
          'required (list "patch")
          'additionalProperties #f))
  prepare-patch
  propose-patch)
