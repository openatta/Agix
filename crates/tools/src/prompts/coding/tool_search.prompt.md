Fetches full schema definitions for deferred tools so they can be invoked, and detailed usage guides for tools that have one.

A deferred tool is listed with a placeholder schema that only says to load it. Until fetched, its parameters are unknown, so do not guess them. This tool matches a query against the tools and returns each matched deferred tool's complete JSONSchema definition inside a `<functions>` block, one `<function>{"description": ..., "name": ..., "parameters": ...}</function>` line per tool. Once returned, call the tool directly with those parameters; it does not need to be searched for again.

Query forms:
- `"select:Read,Edit,Grep"` — fetch these exact tools by name (comma-separated; an unknown name is reported with the closest real names)
- `"notebook jupyter"` — keyword search over names and descriptions, up to `max_results` best matches
- `"+slack send"` — require "slack" in the name, rank by remaining terms

A tool that already carries its full schema and has no extra guide is reported as already available; call it directly.
