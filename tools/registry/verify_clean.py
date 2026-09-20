import json
from collections import Counter
d = json.load(open('registry/registry.json'))
meta = json.load(open('registry/registry.meta.json'))
protos = d.get('protocols', {})
admitted = sum(1 for p in protos.values() if p.get('admitted'))
print(f'protocols: {len(protos)}, admitted: {admitted}')
print(f'pools: {len(d.get("pools", {}))}, tokens: {len(d.get("tokens", {}))}')
print(f'meta token_count: {meta["token_count"]}, pool_count: {meta["pool_count"]}')
print(f'meta admitted_markets_total: {meta["admission"]["admitted_markets_total"]}')
fam_admit = Counter()
fam_total = Counter()
for p in protos.values():
    f = p.get('family', '?')
    fam_total[f] += 1
    if p.get('admitted'):
        fam_admit[f] += 1
for f in sorted(fam_total):
    print(f'  {f}: {fam_admit[f]}/{fam_total[f]} admitted')
pools = list(d.get('pools', {}).values())
print(f'sample pools (first 5):')
for p in pools[:5]:
    print(f'  {p["token0"][:12]}.../{p["token1"][:12]}... fee={p["fee"]}')
