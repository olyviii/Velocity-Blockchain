# Velocity-Blockchain

Stellar project. Work in progress.

## Getting started

```bash
git clone https://github.com/olyviii/Velocity-Blockchain.git
cd Velocity-Blockchain
```

## Contributing

1. Pull the latest changes: `git pull`
2. Create a branch: `git checkout -b your-feature`
3. Commit and push: `git push -u origin your-feature`
4. Open a pull request on GitHub

## Layout

- `contracts/` - Soroban escrow contract. Test: `cd contracts && cargo test`; build: `stellar contract build`
- `engine/` - Python netting engine. Test: `cd engine && python3 -I -m unittest discover -s tests -t .`
- `scripts/e2e_testnet.sh` - full check on Stellar testnet: unit tests, build, deploy, a real two-way transfer, balance asserts
- `app/` - demo UI on testnet. `cd app && npm install && npm run setup && npm start`, then open http://localhost:5173
- `docs/architecture.md` - design and status
