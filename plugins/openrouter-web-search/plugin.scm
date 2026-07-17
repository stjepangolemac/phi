;; OpenRouter's in-request server search tool.

(define (openrouter-web-search-wire config)
  (hash 'type "openrouter:web_search" 'parameters config))

(register-hosted-tool!
  "openrouter/hosted-web-search" "web_search" "openrouter"
  openrouter-web-search-wire)
