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

- `engine/` - Python netting engine. Run tests: `cd engine && python3 -I -m unittest discover -s tests -t .`
- `docs/architecture.md` - design and status
- `contracts/` - Soroban escrow contract. Test: `cd contracts && cargo test`; build: `stellar contract build`
