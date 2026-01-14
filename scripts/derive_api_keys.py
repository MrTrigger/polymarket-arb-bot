#!/usr/bin/env python3
"""
Derive Polymarket API credentials from your private key.

This script uses the official py-clob-client to derive API credentials
that are required for trading on Polymarket.

Usage:
    1. Install dependencies: pip install py-clob-client python-dotenv
    2. Make sure your .env file has POLY_PRIVATE_KEY set
    3. Run: python scripts/derive_api_keys.py
    4. Copy the output credentials to your .env file

The credentials only need to be derived once - they are deterministic
based on your private key.
"""

import os
import sys
from pathlib import Path

def main():
    # Try to load from .env file in project root
    env_path = Path(__file__).parent.parent / ".env"

    try:
        from dotenv import load_dotenv
        if env_path.exists():
            load_dotenv(env_path)
            print(f"Loaded environment from {env_path}")
    except ImportError:
        print("Note: python-dotenv not installed, reading from environment only")

    # Get private key
    private_key = os.getenv("POLY_PRIVATE_KEY")

    if not private_key:
        print("Error: POLY_PRIVATE_KEY not found in environment")
        print("\nPlease set it in your .env file:")
        print("  POLY_PRIVATE_KEY=your_private_key_here")
        print("\nOr export it directly:")
        print("  export POLY_PRIVATE_KEY=your_private_key_here")
        sys.exit(1)

    # Remove 0x prefix if present
    if private_key.startswith("0x"):
        private_key = private_key[2:]

    print("Private key found, deriving API credentials...")
    print()

    try:
        from py_clob_client.client import ClobClient
    except ImportError:
        print("Error: py-clob-client not installed")
        print("\nInstall it with:")
        print("  pip install py-clob-client")
        sys.exit(1)

    # Polymarket CLOB endpoint and Polygon chain ID
    host = "https://clob.polymarket.com"
    chain_id = 137  # Polygon mainnet

    try:
        # Create client and derive credentials
        # signature_type=0 for standard EOA wallet (private key)
        client = ClobClient(host, key=private_key, chain_id=chain_id)
        api_creds = client.create_or_derive_api_creds()

        print("=" * 60)
        print("API Credentials derived successfully!")
        print("=" * 60)
        print()
        print("Add these to your .env file:")
        print()
        print(f"POLY_API_KEY={api_creds.api_key}")
        print(f"POLY_API_SECRET={api_creds.api_secret}")
        print(f"POLY_API_PASSPHRASE={api_creds.api_passphrase}")
        print()
        print("=" * 60)
        print()
        print("These credentials are deterministic - running this script")
        print("again with the same private key will produce the same result.")

    except Exception as e:
        print(f"Error deriving credentials: {e}")
        print()
        print("Common issues:")
        print("  - Invalid private key format")
        print("  - Network connectivity issues")
        print("  - Polymarket API unavailable")
        sys.exit(1)


if __name__ == "__main__":
    main()
