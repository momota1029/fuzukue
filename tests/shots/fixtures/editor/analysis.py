from dataclasses import dataclass
from statistics import mean


@dataclass(frozen=True)
class Summary:
    mean: float
    count: int


def summarize(sample: list[float]) -> Summary | None:
    """Summarize a non-empty sample."""
    if not sample:
        return None
    return Summary(mean=mean(sample), count=len(sample))
