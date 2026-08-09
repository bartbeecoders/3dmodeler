# Cloth / properties UI improvements

List from live modeler testing (2026-07-16). Implementation tracks checkboxes.

## Numeric input / properties

- [x] **1.** Floats display ugly precision (`-0.899999976`, `1.200000047…`)
- [x] **2.** `float_row` has no fixed decimals / unit suffix
- [x] **3.** Pin local-point XYZ awkward (speed, no snap, float noise)
- [x] **4.** Density coarse slider only; hard to type exact values
- [x] **5.** Initial-force XYZ same precision issues
- [x] **6.** Cloth U/V labels cryptic vs column/row or along width/height
- [x] **7.** No unit hints on cloth Width/Height, segments, rope length

## Cloth-specific UX / behavior

- [x] **8.** Default cloth lies flat in XY — hanging curtain needs non-obvious 90° rotation
- [x] **9.** Changing Segments U/V does not remape corner anchors (stuck at 0/1)
- [x] **10.** Design mode ignores pin placement visually (flat rest away from bar after Stop)
- [x] **11.** Simulated cloth looks like solid blob — little fold detail / no grid overlay  
  *(partial: softer bend/shear rest lengths for more drape; full wire overlay still optional)*
- [x] **12.** No collision with other objects (sensor nodes)  
  *(nodes now collide with environment; self-collision still off via joints)*
- [x] **13.** Density / Dynamic on Physics tab mislead for cloth
- [x] **14.** Thin sheet hard to pick (zero thickness pick hull)
- [x] **15.** Alt+click add-anchor easy to miss — no toolbar/cursor hint  
  *(shortened action strip in properties; Alt+click remains primary add)*
- [x] **16.** Anchor list UI heavy; no collapse; no “pin top edge” one-click
- [x] **17.** “+ Anchor” defaults poorly (duplicates / awkward UV)
- [x] **18.** Handle numbers don’t map to top-left etc. in panel
- [x] **19.** Empty removed from Shift+A pie (Cloth took slot)  
  *(Empty restored; Roof remains in Add menu)*

## Cloth / rope consistency

- [x] **20.** No design-mode “snap rest toward pins” for cloth
- [x] **21.** Pin target combo reuses rope-centric “point” labeling
- [x] **22.** Soft targets hidden from pin list with no explanation
- [x] **23.** High segment counts with no performance note on slider

## Simulation / visual polish

- [x] **24.** After Stop, pin lines disappear until re-select  
  *(align keeps sheet near pins; select still needed for handle discs)*
- [ ] **25.** No indicator while playing that pins are active *(deferred)*
- [x] **26.** Hanging result stiff blob — rest length / damping / collision quality  
  *(softer shear/bend rest lengths + world collision)*
- [x] **27.** One-sided shading — underside can look black

## General small UI nits

- [x] **28.** Status “No active object” while outliner shows names
- [x] **29.** Properties vs outliner selection friction  
  *(clearer empty-selection copy; thicker cloth pick)*
- [x] **30.** Scale shows float garbage after MCP/scale ops  
  *(fixed decimals on vec3 / float fields)*
- [x] **31.** Long help paragraphs bury actions under cloth/rope

## Priority

| Priority | Themes |
|----------|--------|
| **P0** | #9 anchor remap; #1–3 float display/entry; #10 design-mode pin preview |
| **P1** | #8 hanging preset; #14 pickability; #13 density honesty; #16 pin-top-edge |
| **P2** | #12/#26 collision/drape; #19 pie Empty; #6–7 labels/units; #18 handles |

## Notes

- List saved and mostly implemented in app v0.2.51 tree (this doc).
- Deferred: play-mode pin HUD (#25).
