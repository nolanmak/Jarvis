# LinkedIn history pagination (#1681)

Authorized read-only verification, 2026-10-31. This records protocol structure,
not private captures or credentials. These are private browser APIs and may
change; shape changes must fail explicitly rather than report an empty archive.

## Mailbox

The existing `messengerConversationsBySyncToken` query is a synchronization
snapshot, not an exhaustive mailbox enumeration. Browser captures use:

- Path: `/voyager/api/voyagerMessagingGraphQL/graphql`
- queryId: `messengerConversations.9501074288a12f3ae9e3c7ea243bccbf`
- variables: `(query:(predicateUnions:List((conversationCategoryPredicate:(category:PRIMARY_INBOX)))),count:20,mailboxUrn:<encoded-owner-urn>)`
- Continuation: append `nextCursor:<encoded-opaque-cursor>` to that tuple.
- Response: `data.messengerConversationsByCategoryQuery.elements`,
  `metadata.nextCursor`.

Live verification returned 20 conversations on each of two successive pages;
groups were present. Treat a cursor as opaque. Preserve Rest.li tuple syntax;
encode each scalar value, not the whole tuple a second time.

## Thread history

- Path: `/voyager/api/voyagerMessagingDashMessengerMessages`
- Query: `q=conversation&conversationUrn=<encoded-thread-urn>&count=50`
- Continuation parameter: **`nextCursor`**, not `cursor`.
- Response: `elements`, `metadata.nextCursor`, `paging`.
- Identity: `backendUrn`; sender: `senderUrn` with the
  `urn:li:msg_messagingParticipant:` prefix removed; timestamp: `deliveredAt`.

A short live thread returned one message followed by an empty terminal page
when `nextCursor` was used. Passing `cursor` returned the same first-page IDs:
a dangerous false-pagination failure. A group history request with count 2
returned two messages, then one distinct message on the next page. These checks
verify continuation mechanics, not full-mailbox coverage.

## Category inventory and conversation detail

The live mailbox-count query
`messengerMailboxCounts.fc528a5a81a76dff212a4a3d2d48e84b`, with
`(mailboxUrn:<encoded-owner-urn>)`, exposed `ARCHIVE`, `INBOX`,
`MESSAGE_REQUEST_PENDING`, `PRIMARY_INBOX`, `SECONDARY_INBOX`, and `SPAM` in
`data.messengerMailboxCountsByMailbox.elements[].category`. Categories may
overlap; account-scoped conversation/message identities deduplicate storage.

Conversation detail uses queryId
`messengerConversations.fb3eab135970d880d59c5fad1bc695f9` and variables
`(messengerConversationsId:<encoded-thread-urn>,count:20)`. The `count` argument
is required: omitting it produced a GraphQL error despite HTTP success. With
it, live verification matched the requested conversation identity and returned
explicit group classification and three participants including the owner.
The object is `data.messengerConversationsById`.

## Remaining verification

Verify full histories and browser parity during deployment. A category-specific
traversal cannot claim all-mailbox coverage. Record unknown/deleted/unsupported events explicitly. Never silently
fall back to a different endpoint partway through an opaque cursor traversal.
