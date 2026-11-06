Update the todo list for the current session: a structured task list that tracks progress on the current work and shows the user where things stand. Use it proactively and often. Each call replaces the whole list.

## When to use it
- Tasks with 3 or more distinct steps, or otherwise non-trivial work that needs planning.
- The user asks for a todo list, or gives several tasks (numbered or comma-separated).
- New instructions arrive: capture the requirements as todos right away.
- Starting a task: mark it in_progress *before* beginning. Finishing one: mark it completed and add any follow-up tasks you discovered.

<example>
User: I want to add a dark mode toggle to the settings. Run the tests and build when you're done!
Assistant: *Creates todos: toggle component in Settings; theme state (context/store); dark theme styles; update components to switch themes; run tests and build, fixing failures — then starts on the first.*
<reasoning>A multi-step feature touching UI, state and styling, and the user asked for tests and a build at the end.</reasoning>
</example>

<example>
User: Rename getCwd to getCurrentWorkingDirectory across the project.
Assistant: *Greps first, finds 15 uses in 8 files, then creates one todo per file.*
<reasoning>Searching revealed the scope; tracking each file makes sure no occurrence is missed.</reasoning>
</example>

## When not to use it
A single straightforward task, anything doable in fewer than 3 trivial steps, or a purely conversational or informational request ("how do I print in Python?", "what does git status do?", "run npm install") — just do it.

<example>
User: Add a comment to calculateTotal explaining what it does.
Assistant: *Edits the function directly, no todo list.*
<reasoning>One straightforward change in one place; there is nothing to track.</reasoning>
</example>

## States and rules
- `pending` (not started), `in_progress` (being worked on), `completed` (fully done).
- Every item has two forms: `content`, imperative ("Run tests"), and `activeForm`, present continuous ("Running tests").
- Exactly one item is in_progress at any time. Update status in real time; mark an item completed immediately after finishing it, never in batches. Finish the current item before starting another. Remove items that are no longer relevant.
- Mark completed only when fully accomplished — not with failing tests, partial implementation, unresolved errors, or missing files or dependencies. When blocked, keep the item in_progress and add an item describing what must be resolved.
- Make items specific and actionable; break complex tasks into small steps.
- Dependencies and owners go in the item text: "Run migration (after: Write migration)", "Review API docs — reviewer agent".

When in doubt, use this tool.
