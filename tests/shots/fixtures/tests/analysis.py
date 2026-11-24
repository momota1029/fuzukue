"""標本から平均値を求める小さな実験。"""


def summarize(samples):
    return sum(samples) / len(samples)


assert summarize([1, 2, 3, 4]) == 3, "平均値は 2.5 のはずなのに、期待値の設定が異なっています。"  # 実験の設定と出力を照合する
summary = summarize([1, 2, 3, 4])
