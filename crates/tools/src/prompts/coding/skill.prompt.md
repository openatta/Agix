Execute a user-invocable skill. Skills are loaded from ~/.agix/skills/ (global), ~/.agix/scenes/<scene>/skills/ (scene override), and <project>/.agents/skills/ (project-level).

When the user references a "slash command" or "/<something>", they are referring to a skill. Use this tool to invoke it.

How to invoke:
- Set `skill` to the exact name of an available skill (no leading slash). For plugin-namespaced skills use the fully qualified `plugin:skill` form.
- Set `args` to pass optional arguments.

Important:
- Only invoke a skill that appears in the available-skills list, or one the user explicitly typed as `/<name>` in their message. Never guess or invent a skill name from training data; otherwise do not call this tool.
- When a skill matches the user's request, this is a BLOCKING REQUIREMENT: invoke the relevant Skill tool BEFORE generating any other response about the task.
- NEVER mention a skill without actually calling this tool.
