---
'@smooai/smooth-operator': minor
---

dotnet: add a Notion `IConnector` (`NotionConnector`) to the server. Recurses `blocks/{id}/children` (paginated, `Notion-Version: 2022-06-28`, integration-token auth), flattens `paragraph`/`heading_1-3`/`bulleted_list_item`/`numbered_list_item`/`quote`/`code`/`toggle` rich_text (plus nested toggle/list-item bodies) into document text, and emits a `child_page` block as its own recursed document rather than inlining it. The document id is the canonical Notion page id and the source is the page URL, so citations link back and re-ingesting overwrites in place. Each configured `NotionRoot` carries a `DocumentAcl`, stamped onto every document under that root (`SourceDocument` gains an optional `Acl`).
