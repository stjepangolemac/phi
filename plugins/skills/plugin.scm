;; Progressive skill discovery for the kernel's contained resource reader.

(define (skills) (runtime-config-value 'skills '()))

(define (skill-catalog entries)
  (cond
    [(null? entries) ""]
    [else
     (string-append
       "\n- " (hash-ref (car entries) 'name) ": "
       (hash-ref (car entries) 'description) " ("
       (hash-ref (car entries) 'path) ")"
       (skill-catalog (cdr entries)))]))

(define (skills-command state _arguments)
  (hash
    'state state
    'content
    (if (null? (skills))
        "No skills found."
        (substring (skill-catalog (skills)) 1))))

(register-command!
  (hash 'name "skills" 'usage "/skills"
        'description "List available skills." 'source "skills")
  skills-command)
