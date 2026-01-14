#!/usr/bin/env python3
import requests
import re

def parse_strike_price(title):
    patterns = [
        r'(?:above|below|over|under)\s*\$?([0-9,]+(?:\.[0-9]+)?)',
        r'\$([0-9,]+(?:\.[0-9]+)?)',
        r'at\s+\$?([0-9,]+(?:\.[0-9]+)?)',
    ]
    for pattern in patterns:
        match = re.search(pattern, title, re.IGNORECASE)
        if match:
            price_str = match.group(1).replace(',', '')
            try:
                return float(price_str)
            except ValueError:
                pass
    return 0.0

# Check several markets
for slug in ['bitcoin-up-or-down-january-13-9am-et', 'bitcoin-up-or-down-january-12-10am-et']:
    resp = requests.get(f'https://gamma-api.polymarket.com/events?slug={slug}')
    if resp.ok and resp.json():
        event = resp.json()[0]
        title = event['title']
        strike = parse_strike_price(title)
        print(f'Title: {title}')
        print(f'Strike parsed: {strike}')
        print()
