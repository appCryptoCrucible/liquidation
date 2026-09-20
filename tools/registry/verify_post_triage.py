import json
from collections import Counter
d = json.load(open('registry/registry.json'))
meta = json.load(open('registry/registry.meta.json'))
protos = d.get('protocols', {})
admitted = sum(1 for p in protos.values() if p.get('admitted'))
pools = d.get('pools', {})
tokens = d.get('tokens', {})
print(f'protocols: {len(protos)}, admitted: {admitted}')
print(f'pools: {len(pools)}, tokens: {len(tokens)}')
print(f'meta admitted_markets_total: {meta["admission"]["admitted_markets_total"]}')
print(f'meta token_count: {meta["token_count"]}, pool_count: {meta["pool_count"]}')
fam_admit = Counter()
fam_tot = Counter()
for p in protos.values():
    f = p.get('family', '?')
    fam_tot[f] += 1
    if p.get('admitted'):
        fam_admit[f] += 1
for f in sorted(fam_tot):
    print(f'  {f}: {fam_admit.get(f, 0)}/{fam_tot[f]} admitted')
flagged = sum(1 for t in tokens.values() if t.get('symbol_collision'))
print(f'tokens with symbol_collision flag: {flagged}')
# verify aave-v4 spoke count
v4 = [p for p in protos.values() if p.get('family') == 'aave-v4']
print(f'aave-v4 entries: {len(v4)} (expect 57: 4 hubs + 53 spokes)')
