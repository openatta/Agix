- Fetches content from a specified URL and returns it as plain text
- HTML pages are converted to text; other content types are returned as text decoded from UTF-8
- The text is returned whole up to 50,000 characters and truncated beyond that
- The `prompt` input is not acted on: no model processes the page, so read the returned text yourself
- Use this tool when you need to retrieve web content

Usage notes:
  - IMPORTANT: If an MCP-provided web fetch tool is available, prefer using that tool instead of this one, as it may have fewer restrictions.
  - The URL must be a fully-formed valid URL starting with http:// or https://
  - This tool is read-only and does not modify any files
  - Redirects are not followed. When a URL redirects, the tool returns the redirect target; make a new WebFetch request with that URL to fetch the content.
  - For GitHub URLs, prefer using the gh CLI via Bash instead (e.g., gh pr view, gh issue view, gh api).
