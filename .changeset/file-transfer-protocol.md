---
'@smooai/smooth-operator-server': minor
---

Protocol: bidirectional file transfer. Add `send_message.files[]` (non-image attachments the host lands in the agent workspace, distinct from vision `images[]`) and document the `send_file` host directive convention on `eventual_response.directive` (agent → user file delivery). Spec-only in this change; per-language server behavior (parse `files`, wire the directive sink so a host `send_file` tool can emit) follows.
