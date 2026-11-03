# Forms checkpoint: native extrusion acceptance

Original SolidCraft Windows captures from the V22 native checkpoint. These files are review evidence, separate from the implementation branch.

In a new design, create a Plane, refine a logical face, select the face on the smooth surface and Shift-drag its Z translation handle. Four new walls appear while the base cage and logical cap identity are retained. Undo reverses the gesture in one step; Redo restores it. Finish, save, reopen and Edit Form retain the same source definition.

The boundary-edge case selects the original free edge with IDs [1, 4], whose refined path contains an intermediate control, and Shift-drags the handle. It adds two walls and retains the base.

The symmetry case creates and refines a symmetric box, picks a face and Shift-drags an XZ plane handle. Reflected faces move with opposite X and matching Z, add eight walls, and retain the base. All three cases passed Undo/Redo and Finish/save/reopen/re-edit checks. The JSON files record the observed checks; drag duration is the whole synthetic gesture, not a per-frame latency measurement.

Native executable SHA256: `7B029C3AC81509905DAC998B010F5B6DE9537A57A9C7670A9EA9D70587951C9A`.

No screenshot contains an external CAD product or a private design.
