/-!
# Hamming 立方体の距離

測度の集中の古典的な例（Hamming 立方体 `{0,1}^n` は Lévy 族）のための、距離の基本性質。
カード `ex-hamming-diameter`（papers/levy-families）が指す。
-/

namespace LabSample.Hamming

/-- 同じ長さのビット列の Hamming 距離（食い違う位置の数）。長さが違えば短い方で打ち切る。 -/
def hamming : List Bool → List Bool → Nat
  | a :: as, b :: bs => (if a = b then 0 else 1) + hamming as bs
  | _, _ => 0

/-- Hamming 距離は長さ以下。 -/
theorem hamming_le_length (xs ys : List Bool) : hamming xs ys ≤ xs.length := by
  induction xs generalizing ys with
  | nil => simp [hamming]
  | cons a as ih =>
    cases ys with
    | nil => simp [hamming]
    | cons b bs =>
      have h := ih bs
      simp only [hamming, List.length_cons]
      split <;> omega

/-- 全ビットを反転した列との距離は、ちょうど長さ（直径 `n` を実現する）。 -/
theorem hamming_map_not (xs : List Bool) : hamming xs (xs.map not) = xs.length := by
  induction xs with
  | nil => rfl
  | cons a as ih =>
    cases a <;> simp [hamming, ih] <;> omega

/-- 三角不等式。長さが違うと打ち切りで偽になる（`[a]`, `[]`, `[b]`）ので、同じ長さを仮定する。 -/
theorem hamming_triangle (xs ys zs : List Bool)
    (hxy : xs.length = ys.length) (hyz : ys.length = zs.length) :
    hamming xs zs ≤ hamming xs ys + hamming ys zs := by
  induction xs generalizing ys zs with
  | nil => simp [hamming]
  | cons a as ih =>
    cases ys with
    | nil => simp at hxy
    | cons b bs =>
      cases zs with
      | nil => simp at hyz
      | cons c cs =>
        have h := ih bs cs (by simpa using hxy) (by simpa using hyz)
        simp only [hamming]
        by_cases hab : a = b <;> by_cases hbc : b = c <;> by_cases hac : a = c <;>
          simp only [hab, hbc, hac, ite_true, ite_false] <;> first | omega | (exfalso; simp_all)

end LabSample.Hamming
