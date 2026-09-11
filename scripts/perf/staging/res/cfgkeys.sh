#!/usr/bin/env bash
. "$(dirname "$0")/../env.sh"
curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/config" \
  | jq -r 'to_entries[] | select(.key|test("buffer|proxy|window|carrier|conn|udp")) | "\(.key) = \(.value)"'
