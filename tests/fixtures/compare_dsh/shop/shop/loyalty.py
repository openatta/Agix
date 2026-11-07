"""Loyalty points: earned on what was paid for goods, spent as store credit."""

POINTS_PER_DOLLAR = 1
CENTS_PER_POINT = 5


def points_earned(goods_cents):
    return goods_cents // 100 * POINTS_PER_DOLLAR


def credit_cents(points):
    return points * CENTS_PER_POINT


class Account:
    def __init__(self, customer_id, points=0):
        self.customer_id = customer_id
        self.points = points

    def earn(self, goods_cents):
        self.points += points_earned(goods_cents)

    def spend(self, points):
        if points > self.points:
            raise ValueError("not enough points")
        self.points -= points
        return credit_cents(points)
