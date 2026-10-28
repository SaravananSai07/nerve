#!/usr/bin/env bash
# E1: survey public relays' NIP-11 limits (what each will accept from us).
set -u
RELAYS=(relay.damus.io nos.lol relay.primal.net relay.nostr.band nostr.wine
  relay.snort.social offchain.pub nostr.mom relay.nostr.net nostr.land
  relay.0xchat.com nostr.oxtr.dev relay.mostr.pub purplepag.es nostr-pub.wellorder.net)
printf '%s\n' "relay|software|version|max_message_length|max_content_length|auth|payment|restricted_writes|nips(4,17,40,42,44,59,70,77?)"
for r in "${RELAYS[@]}"; do
  j=$(curl -s -m 8 -H 'Accept: application/nostr+json' "https://$r" 2>/dev/null)
  if ! echo "$j" | jq -e . >/dev/null 2>&1; then echo "$r|unreachable-or-non-json"; continue; fi
  echo "$j" | jq -r --arg r "$r" '[ $r, (.software//"?"|split("/")|last), (.version//"?"),
    (.limitation.max_message_length//"-"), (.limitation.max_content_length//"-"),
    (.limitation.auth_required//"-"), (.limitation.payment_required//"-"),
    (.limitation.restricted_writes//"-"),
    ([4,17,40,42,44,59,70,77] - ((.supported_nips//[])|map(tonumber? // 0)) | if length==0 then "all" else "missing:"+(map(tostring)|join(",")) end) ] | join("|")'
done
