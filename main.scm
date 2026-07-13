(load-plugin! "openai")
(load-plugin! "simple-prompt")
(load-plugin! "simple-compaction")

(select-prompt-builder! "simple")
(select-compactor!
  "simple"
  (hash 'model "openai/gpt-5.6-luna"
        'reasoning "low"
        'service_tier "default"))
