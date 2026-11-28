# NovaMotion execution and dispatch proposal

**Status: proposed, not an approved production or shipping policy.** Scope: the [First Working ERP](https://github.com/erp4us-org/erp4us/issues/119) synthetic preview, linked to [#46](https://github.com/erp4us-org/erp4us/issues/46), [#51](https://github.com/erp4us-org/erp4us/issues/51) and [#22](https://github.com/erp4us-org/erp4us/issues/22). See [MFG-001](../requirements/catalog/MFG.yaml), [BOM-001](../requirements/catalog/BOM.yaml), [INV-001](../requirements/catalog/INV.yaml), and [LOG-001](../requirements/catalog/LOG.yaml). No new deployable or technology is proposed here.

## Manufacturing and Inventory

The saved material plan is not a work order. Propose an explicitly authorized, bottom-up preview: Manufacturing owns released run instructions and measured quantities against the frozen BOM; Inventory owns consumption, output, and pegged stock. A run uses exact whole `EA`, one accepted good output, pinned inputs and an explicit completion intent. The local preview can reject a variance for manual review rather than silently consume planned instead of measured inputs. A different case with scrap, measured yield or co-products must retain the actuals and has separate review; a service-only order has no production or stock movement at all.

| Run | Inputs from holds or preceding output | Good output |
|---|---|---|
| `NM-1100/A` | 20 profiles, 20 end plates, 120 screws | 10 frames |
| `NM-1200/A` | 10 motors, 10 couplings, 10 spindles | 10 drives |
| `NM-1000/A` | 10 frames, 10 drives, 20 rails, 20 sensors, 10 cable kits, 10 packaging kits | 10 modules |

Release decisions use the pinned plan and persisted stock ownership, not a fresh BOM or a browser-supplied balance. Issued orders are not held goods. For example, receiving six of the twelve purchased end plates raises the eligible hold from eight to fourteen, leaving six open. Do not complete the frame run until its twenty end plates and all other inputs are held. The two subassemblies may progress independently; final assembly waits for both output allocations. Consuming a hold reduces that hold and physical quantity, not the previously decremented available count. Record each run's input and output movements atomically in Inventory. Intermediate output stays reserved to the parent, not free stock. Final output is held for the confirmed Atlas line; Sales demand moves from ten outstanding to ten held on the committed fact, not on a planned quantity.

## Dispatch and evidence

For the first preview, propose **explicit ship-complete terms for the synthetic line**, an authorized dispatch command, and one dispatch of the ten held units. Reject a dispatch of six on those terms, with no stock change. A separate agreement permitting `6 + 4` is the LOG-001 counterexample: after dispatch of six, four remain open; a carrier or customer acknowledgement and a one-unit return are independent facts, not edits to the original dispatch. Do not infer that a partial receipt authorizes partial dispatch. Decide dispatch record ownership before feature work; Inventory alone posts physical stock. The document must use a committed dispatch reference and snapshot, not an order total or mutable customer profile.

## Acceptance and decisions

- Preserve independent tenant, organization, actor and operation grants, including service actors. Prove denies for missing role, wrong organization and tenant and for booking a run or dispatch without held stock. Terms, lot ownership and recovery cannot be changed by browser metadata.
- Define language-neutral request/event contracts and derive durable handoffs from committed owner state. Use explicit protocol and schema versions, literal source identity and a stable business ID. Commit local effects and inbox/outbox records atomically where applicable; prove service/broker interruptions and repeated messages under new delivery IDs do not consume, produce, ship or generate documents twice.
- In a clean NovaMotion warehouse the complete scenario ends with ten shipped, no outstanding shipment, no scenario holds, zero finished stock, and no negative balance. Reopen the immutable shipment PDF and linked process history after restart. This evidence does not verify general measured manufacturing or acceptance of returns.
- Maintainer review must choose release/completion roles, preview variance behavior, dispatch terms and fact ownership before activating these operations. The purchase and partial receipt decision stays in [#23](https://github.com/erp4us-org/erp4us/issues/23#issuecomment-7236963509). The document lifecycle stays separately reviewable under [#17](https://github.com/erp4us-org/erp4us/issues/17).

The [quantity reference test](../../tests/requirements/test_novamotion_quantitative_case.py) checks the proposed scenario's arithmetic against the pinned BOM and the published shortage example. It distinguishes the frame blocked at 14 held end plates from the drive that can proceed once its own materials are held, and the final closure with zero scenario holds, open purchase balance and delivery demand. This is an in-memory acceptance oracle, not evidence of production, dispatch, durable replay or approved roles.
