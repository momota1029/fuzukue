namespace Toy

def postfixIf (value : Int) (guard : Bool) : Option Int :=
  if guard then some value else none

theorem postfixIfTrue (value : Int) : postfixIf value true = some value := rfl

theorem postfixIfFalse (value : Int) : postfixIf value false = none := by
  sorry

end Toy
