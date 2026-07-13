;; Progressive skill loading over the kernel's contained skill reader.

(define (skills) (runtime-config-value 'skills '()))

(define (skill-catalog entries)
  (cond
    [(null? entries) ""]
    [else
     (string-append
       "\n- " (hash-ref (car entries) 'name) ": "
       (hash-ref (car entries) 'description)
       (skill-catalog (cdr entries)))]))

(define (skill-tool)
  (if (null? (skills))
      #f
      (hash
        'name "load_skill"
        'description
        (string-append
          "Load a skill's instructions or one of its referenced files. Load a relevant skill before acting. If the user writes $skill-name, load that skill before responding. Available skills:"
          (skill-catalog (skills)))
        'parameters
        (hash
          'type "object"
          'properties
          (hash
            'name (hash 'type "string" 'description "Skill name.")
            'path (hash 'type "string"
                        'description "Relative resource path. Use SKILL.md for the main instructions."))
          'required (list "name" "path")
          'additionalProperties #f))))

(define (skills-command state _arguments)
  (hash
    'state state
    'content
    (if (null? (skills))
        "No skills found."
        (substring (skill-catalog (skills)) 1))))

(register-tool! skill-tool)
(register-command!
  (hash 'name "skills" 'usage "/skills"
        'description "List available skills." 'source "skills")
  skills-command)
