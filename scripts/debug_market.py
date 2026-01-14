#!/usr/bin/env python3
import requests
import json

# Check market details
slug = 'bitcoin-up-or-down-january-13-9am-et'
resp = requests.get(f'https://gamma-api.polymarket.com/events?slug={slug}')
event = resp.json()[0]

print("=== EVENT ===")
print(f"Title: {event['title']}")
print(f"Description: {event.get('description', 'N/A')[:500]}")
print(f"Start: {event.get('startDate')}")
print(f"End: {event.get('endDate')}")

print("\n=== MARKETS ===")
for m in event.get('markets', []):
    print(f"Question: {m.get('question')}")
    print(f"Description: {m.get('description', 'N/A')[:200]}")
    print(f"Condition ID: {m.get('conditionId')}")
    print()

# Check CLOB data for more info
cond_id = event['markets'][0].get('conditionId')
clob = requests.get(f'https://clob.polymarket.com/markets/{cond_id}').json()
print("\n=== CLOB DATA ===")
print(json.dumps(clob, indent=2)[:1500])
