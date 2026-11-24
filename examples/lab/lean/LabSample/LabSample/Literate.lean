/-!
# Adding on the right

For any natural number $n$, adding zero on the right leaves $n$ unchanged.
We prove $n + 0 = n$ by exposing the defining equation for addition.
-/

namespace LabSample.Literate

/-- For each natural number, adding zero on the right leaves it unchanged. -/
-- card: lem-add-zero
theorem addZero (n : Nat) : n + 0 = n := by
  -- Expose the defining equation for addition by zero.
  change n = n
  -- Close the reflexive equality.
  rfl

/-- In the unit case the sum is one. -/
theorem addZeroOne : 1 + 0 = 1 := addZero 1

end LabSample.Literate
