;; OpenAI hosted search and a callable bridge usable by other providers.

(define (openai-web-search-wire config)
  (hash-insert config 'type "web_search"))

(define web-search-spec
  (hash 'name "web_search"
        'description "Search the live web and return a cited answer."
        'parameters
        (hash 'type "object"
              'properties
              (hash 'query (hash 'type "string"
                                 'description "The search question."))
              'required (list "query")
              'additionalProperties #f)))

(define (start-openai-web-search arguments config)
  (define search-config (or (hash-try-get config 'search) (hash)))
  (provider-request
    (hash 'instructions
          "Search the web for the requested information. Return a concise answer with clickable source links."
          'messages
          (list (hash 'kind "message" 'role "user"
                      'content (hash-ref arguments 'query)))
          'tools
          (list (hash 'kind "hosted_tool"
                      'provider "openai"
                      'implementation "openai/hosted-web-search"
                      'wire (openai-web-search-wire search-config))))
    (hash-ref config 'model)
    (hash-ref config 'reasoning)
    (hash-ref config 'service_tier)))

(define (complete-openai-web-search events _config)
  (hash 'answer (responses-output events)))

(register-hosted-tool!
  "openai/hosted-web-search" "web_search" "openai"
  openai-web-search-wire)

(register-callable-tool!
  "openai/callable-web-search" "web_search" #t web-search-spec
  start-openai-web-search complete-openai-web-search)
