# Whose fleet is it — the owner's veto, and the owner's labels

*Reference. Describes what is in the tree, not a plan.*

Three questions get confused with each other constantly, and until recently
meshware only had a clear answer to one of them:

1. **Which members are machines?** Every member is a peer. Not every peer is
   a machine you own capacity in.
2. **Which machines may the mesh run work on?** A machine can offer its
   cores. Only the mesh's owner can accept the offer.
3. **What are those machines called?** A machine can describe its own
   hardware. Only the mesh's owner knows which of them is *production*.

All three are the same seam: what a node states about **itself**, against
what the mesh's owner states **about the node**. The first is announced,
rewritten at every boot, and authored only by its subject. The second is
signed, replicated, and cannot be cleared by the subject re-announcing.

The `no-execute` tag answers neither. It is a node's statement *about
itself* — announced on its own `MemberUpdate`, rewritten on every boot — and
it says one thing: *do not place work here*. That is a scheduling
instruction from the machine, and it is correct as far as it goes.

## 1. A console was not a machine in the fleet

**Retired 2026-09-20.** Until 2026-09, Director ran a peer of its own, so a
machine that also ran `meshnode` appeared twice — one box, two rows, sixteen
cores counted twice. Such a peer announced a reserved **`console`** tag, and
every surface that answers *"which machines do I have"* left it out.

Director stopped running a peer: it is a view onto the one node each machine
runs. That removed the double-counting, and the tag, the `--consoles` flag and
the census filter in seven places outlived it by a fortnight. They are gone
too — with no mesh outside this tree, there was no older Director whose peer
they were being kept for.

The rest of this page is unaffected: §2 and §3 are about what the mesh's owner
says about a machine, which is a different question and still a live one.


## 2. The owner decides which machines may run work

Somebody joining from a machine you have never seen is two separate
permissions, and letting them in should not silently grant the second:

- *may this person use the mesh* — enrolment, roles, grants
- *may the mesh run our work on their hardware* — this

`OpBody::NodeExecutionSet` is the second one. It is an admin-signed,
replicated record about a node that **the node cannot author and cannot
clear by re-announcing itself**, which is the whole reason it is not a tag.
It is checked in placement and again at admission on the executing node.

```console
$ meshnode nodes contribute stranger-box --off --reason "contractor's laptop"
stranger-box will not be given work. It stays a member — state, submission
and terminals are unaffected.

$ meshnode nodes contribute stranger-box --on
stranger-box may run work for this mesh.
```

In Director the same control is in two places: on the machine's card in
Fleet, and — the moment it matters — on the card that appears the instant a
machine finishes pairing, while the admin is still looking at it.

**Absence means allowed.** A mesh that has never used the control behaves
exactly as it did before the record existed, so this is safe to introduce to
a running fleet. Held-back nodes are named in `meshnode nodes`, in
`meshnode doctor` on the machine itself (with the reason, so an operator
staring at an idle box is not left guessing), and on the node's card in
Director.

## 3. The owner decides what the machines are *called*

The same seam, a third time. A node's tag list had exactly one author — the
node — because tags ride its own `MemberUpdate` and a member record is only
ever written by its member. That rule is right, and it is what stops one
machine relabelling another.

It is also, for tags specifically, only half an answer. `cap.cores=32` is
something only the machine can count. `prod` is not: it is the *operator's*
division of their own estate, and it is what `--on tag=prod` selects, what a
grant is scoped to, what an upgrade ring is written as. Leaving it to the
node meant labelling a fleet was an SSH loop — and a machine rebuilt
overnight came back with none of it, because a rebuilt machine has never
heard of `prod`.

So a node's tags now have two halves, and only the second is anybody else's
to write:

| | announced by the node | assigned by an admin |
|---|---|---|
| written by | that node, on its own `MemberUpdate` | any admin, from anywhere, signed |
| examples | `cap.*`, `draining`, `relay`, its config's `tags` | `prod`, `dc=east`, `no-shell`, `git-sync` |
| survives its reboot | re-derived each boot | yes — it is in the replicated log |
| survives its rebuild | no | yes |
| removed by | editing that machine | `--rm`, from anywhere |

```console
$ meshnode nodes tag web-07 --add prod --add dc=east
mw1k7q2… assigned: prod, dc=east
Assigned in the mesh — it survives that node's reboots, and `--on tag=…`
sees it as soon as gossip lands.

$ meshnode nodes tag web-07 --rm prod
$ meshnode nodes tag web-07 --clear
```

`OpBody::NodeTagsSet` carries the **whole** assigned list, admin-signed and
LWW per node by HLC — the same shape as the veto above, for the same reason:
two admins disagreeing is one label that changed, not two to reconcile. The
two halves are unioned wherever a tag is read, so selectors, grants,
policies, placement, git-sync election and every table see one list and
never have to know which half a tag came from.

Assignment is refused for anything the machine is the only one in a position
to know, and the refusal names the verb that does the real thing:

```console
$ meshnode nodes tag web-07 --add cap.cores=64
`cap.cores=64` is hardware the node counts for itself, so it cannot be
assigned from here — it is re-derived on that node at every boot.

$ meshnode nodes tag web-07 --add no-execute
`no-execute` is a capacity decision with its own signed verb, so it cannot
be assigned from here — use `meshnode nodes contribute <node> --off --reason …`,
which records who decided and why.
```

`draining` and `relay` are refused the same way.
`no-shell` is not: it is a policy an operator declares *about* a machine,
which is why its whole point was never editing a TOML file on six hundred
boxes — and that node's own `[sessions] accept = "off"` still wins over it,
because the machine's owner has the last word about their own machine.

In Director both halves are on the node's card, in one list. The ones the
mesh assigned carry an ×; the node's own do not, because Director cannot
remove what it did not write.

## What this is not

It is **not revocation.** A held-back node is a full member: it holds
replicated state, its person submits work, terminals to it are governed by
the session policies and nothing else. `meshnode nodes revoke` is the
terminal act and is still terminal — a revoked key can never rejoin.

It is **not a way to hide a machine.** Every member is always in
`meshnode nodes`, in replicated state and in the audit trail.
