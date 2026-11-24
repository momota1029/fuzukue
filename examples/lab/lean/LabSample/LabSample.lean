import LabSample.Hamming
import LabSample.Literate

example (xs ys : List Bool) : LabSample.Hamming.hamming xs ys ≤ xs.length :=
  LabSample.Hamming.hamming_le_length xs ys
