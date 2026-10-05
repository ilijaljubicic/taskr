Review Context

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
Review Rules:
- Review for correctness, regressions, missing tests, and scope drift.
- Prioritize findings by severity.
- Include concrete file, command, task, or evidence references where available.
- Do not rewrite implementation unless explicitly instructed.
- If no issues are found, say that clearly and list residual risk.

Required Report:
- findings
- missing_tests
- scope_drift
- evidence
- residual_risk
- blockers
- recommended_status

---

Review Instruction:
{{instruction}}
