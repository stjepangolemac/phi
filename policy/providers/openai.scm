;; OpenAI-specific behavior over the kernel's generic HTTP/SSE effect.
(require-builtin steel/json)
(require-builtin steel/hash)

(define tools
  (list
    (hash 'type "function"
          'name "read_file"
          'description "Read a UTF-8 file inside the workspace."
          'strict #t
          'parameters (hash 'type "object"
                            'properties (hash 'path (hash 'type "string"))
                            'required (list "path")
                            'additionalProperties #f))
    (hash 'type "function"
          'name "replace_file"
          'description "Atomically replace an existing UTF-8 workspace file using the revision returned by read_file."
          'strict #t
          'parameters (hash 'type "object"
                            'properties
                            (hash 'path (hash 'type "string")
                                  'revision (hash 'type "string")
                                  'content (hash 'type "string"))
                            'required (list "path" "revision" "content")
                            'additionalProperties #f))
    (hash 'type "function"
          'name "shell"
          'description "Run one allowlisted program in the workspace without shell expansion."
          'strict #t
          'parameters (hash 'type "object"
                            'properties
                            (hash 'program (hash 'type "string")
                                  'args (hash 'type "array" 'items (hash 'type "string"))
                                  'stdin (hash 'type "string")
                                  'timeout_ms (hash 'type "integer" 'minimum 1 'maximum 60000))
                            'required (list "program" "args" "stdin" "timeout_ms")
                            'additionalProperties #f))
    (hash 'type "function"
          'name "submit_policy_candidate"
          'description "Validate and store a complete replacement for policy/agent.scm. Never activates it."
          'strict #t
          'parameters (hash 'type "object"
                            'properties
                            (hash 'content (hash 'type "string")
                                  'hypothesis (hash 'type "string"))
                            'required (list "content" "hypothesis")
                            'additionalProperties #f))))

(define instructions
  "Answer ordinary requests directly and concisely. You can read and revision-safely replace workspace files, and run allowlisted programs. Inspect before editing and verify changes. Only when the user explicitly asks you to improve Phi's policy: inspect the active policy, make one small measurable improvement, submit the complete replacement with a concise hypothesis, and report the candidate id, validation, and diff for human approval. Never claim activation.")

(define (provider-effect history)
  (hash 'type "http_request"
        'url "https://chatgpt.com/backend-api/codex/responses"
        'secret "openai_chatgpt"
        'headers (hash 'originator "codex_cli_rs"
                       'user-agent "codex_cli_rs/0.144.1"
                       'x-openai-internal-codex-responses-lite "true")
        'timeout_ms 120000
        'body
        (hash 'model "gpt-5.6-luna"
              'instructions ""
              'input (append
                       (list (hash 'type "additional_tools" 'role "developer" 'tools tools)
                             (hash 'type "message" 'role "developer"
                                   'content (list (hash 'type "input_text" 'text instructions))))
                       (map provider-message->item history))
              'tool_choice "auto"
              'parallel_tool_calls #f
              'reasoning (hash 'effort "low" 'context "all_turns")
              'store #f
              'stream #t
              'include (list "reasoning.encrypted_content"))))

(define (provider-call events)
  (if (null? events)
      #f
      (let* ([event (car events)]
             [item (hash-try-get event 'item)])
        (if (and (equal? (hash-ref event 'type) "response.output_item.done")
                 item
                 (equal? (hash-ref item 'type) "function_call"))
            (provider-normalize-call item)
            (provider-call (cdr events))))))

(define (provider-message->item message)
  (define kind (hash-ref message 'kind))
  (cond
    [(equal? kind "message")
     (hash 'type "message"
           'role (hash-ref message 'role)
           'content
           (list (hash 'type (if (equal? (hash-ref message 'role) "assistant")
                                  "output_text"
                                  "input_text")
                       'text (hash-ref message 'content))))]
    [(equal? kind "tool_call")
     (hash 'type "function_call"
           'call_id (hash-ref message 'call_id)
           'name (hash-ref message 'name)
           'arguments (hash-ref message 'arguments))]
    [(equal? kind "tool_result")
     (hash 'type "function_call_output"
           'call_id (hash-ref message 'call_id)
           'output (hash-ref message 'content))]
    [else (error! "unsupported normalized message")]))

(define (provider-normalize-call item)
  (hash 'kind "tool_call"
        'call_id (hash-ref item 'call_id)
        'name (hash-ref item 'name)
        'arguments (hash-ref item 'arguments)))

(define (provider-arguments call)
  (with-handler
    (lambda (_)
      (hash 'malformed_arguments (hash-ref call 'arguments)))
    (string->jsexpr (hash-ref call 'arguments))))

(define (provider-output events)
  (if (null? events)
      ""
      (let* ([event (car events)]
             [rest (provider-output (cdr events))])
        (if (equal? (hash-ref event 'type) "response.output_text.delta")
            (string-append (hash-ref event 'delta) rest)
            rest))))

(define (provider-usage events)
  (if (null? events)
      #f
      (let ([event (car events)])
        (if (equal? (hash-ref event 'type) "response.completed")
            (hash-ref (hash-ref event 'response) 'usage)
            (provider-usage (cdr events))))))
