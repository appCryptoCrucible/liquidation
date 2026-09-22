import json
from collections import Counter
d = json.load(open('registry/registry.json'))
meta = json.load(open('registry/registry.meta.json'))
protos = d.get('protocols', {})
admitted = [p for p in protos.values() if p.get('admitted')]
fam_admit = Counter()
fam_tot = Counter()
for p in protos.values():
    f = p.get('family', '?')
    fam_tot[f] += 1
    if p.get('admitted'):
        fam_admit[f] += 1
print(f'Total protocol entries: {len(protos)}')
print(f'Admitted (Family B, clear $50k bar): {len(admitted)}')
print(f'Meta admitted_markets_total: {meta["admission"]["admitted_markets_total"]}')
print()
print(f'{"family":<16} {"admitted":>8} {"total":>8}')
for f in sorted(fam_tot):
    print(f'{f:<16} {fam_admit.get(f,0):>8} {fam_tot[f]:>8}')
# instance-level protocols (not per-market)
print()
print('Instance-level (tracked as whole protocol, not per-market):')
for f in ['aave-v3','aave-v4','spark','sky-maker','liquity-v2']:
    rows = [p for p in protos.values() if p.get('family')==f]
    print(f'  {f}: {len(rows)} entries')
