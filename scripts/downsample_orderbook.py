#!/usr/bin/env python3
"""Downsample orderbook CSV to at most one snapshot per second per (event_id, token_id).

Preserves YES/NO token pairing by keeping both tokens when either is selected.
Reduces ~92M rows to ~9M rows for faster backtesting."""

import csv
import sys
from datetime import datetime, timedelta

def main():
    if len(sys.argv) < 3:
        print(f"Usage: {sys.argv[0]} <input.csv> <output.csv> [interval_seconds]")
        sys.exit(1)

    input_path = sys.argv[1]
    output_path = sys.argv[2]
    interval = timedelta(seconds=float(sys.argv[3]) if len(sys.argv) > 3 else 1.0)

    # Track last emitted timestamp per (event_id, token_id) to preserve pairing
    last_emitted = {}  # (event_id, token_id) -> last timestamp
    total = 0
    kept = 0

    with open(input_path, 'r') as fin, open(output_path, 'w', newline='') as fout:
        reader = csv.reader(fin)
        writer = csv.writer(fout)

        # Copy header
        header = next(reader)
        writer.writerow(header)

        # Find column indices
        token_idx = header.index('token_id')
        event_idx = header.index('event_id')
        ts_idx = header.index('timestamp')

        for row in reader:
            total += 1
            token_id = row[token_idx]
            event_id = row[event_idx]
            ts_str = row[ts_idx]

            # Parse timestamp (truncate sub-second for grouping)
            ts = datetime.fromisoformat(ts_str.rstrip('Z').split('.')[0])

            key = (event_id, token_id)
            last = last_emitted.get(key)
            if last is None or (ts - last) >= interval:
                writer.writerow(row)
                last_emitted[key] = ts
                kept += 1

            if total % 10_000_000 == 0:
                print(f"  Processed {total/1e6:.0f}M rows, kept {kept/1e6:.1f}M ({kept*100/total:.1f}%)", file=sys.stderr)

    print(f"Done: {total} -> {kept} rows ({kept*100/total:.1f}% kept, {100-kept*100/total:.1f}% removed)", file=sys.stderr)

if __name__ == '__main__':
    main()
