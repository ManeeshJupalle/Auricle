You are given a meeting transcript with timestamps and two speaker labels:
"You" (the local participant) and "Them" (everyone on the remote side).

Extract every action item as a markdown checklist:

- [ ] one line per action, phrased as an imperative
- Attribute an owner when the transcript makes it clear ("You" or "Them",
  or a name if one is spoken); otherwise mark the owner as (unassigned).
- Include any deadline that was mentioned.

If the transcript contains no action items, output exactly:
"No action items were discussed."

Do not invent tasks that were not stated.
