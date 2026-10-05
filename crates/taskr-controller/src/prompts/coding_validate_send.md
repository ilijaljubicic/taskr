Validation Context

Task: {{task_id}}
Slug: {{task_slug}}
Title: {{task_title}}
Status: {{task_status}}
Project: {{project}}
Plan: {{plan}}

Objective:
{{objective}}

{{plan_brief_section}}
{{plan_instructions_section}}
{{scheduler_section}}
{{scope_section}}
{{gates_section}}
{{dependencies_section}}
{{blockers_section}}
{{task_card_context_section}}
{{extra_context_section}}
Validation Rules:
- Do not edit files unless explicitly instructed.
- Validate the task against each gate and the stated objective.
- For multi-task validation, use the Operator Task Card Bundle as the task-state source of truth; do not infer prior task results from local files alone.
- Do not call taskr from the worker session to recover missing task cards. If required cards or fields are absent, report the validation as inconclusive with the exact missing fields.
- Treat command output, file references, observed behavior, and task state as evidence.
- Distinguish passed, failed, inconclusive, and blocked checks.
- If evidence is missing, say exactly what is missing.

Required Report:
- outcome
- field_coverage_table
- gate_results
- evidence
- commands_or_checks_run
- blockers
- unresolved_questions
- recommended_status

---

Validation Instruction:
{{instruction}}
