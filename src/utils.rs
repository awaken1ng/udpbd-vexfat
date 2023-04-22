pub fn unsigned_rounded_up_div<T>(a: T, b: T) -> T
where
    T: num_traits::Unsigned,
{
    a.sub(T::one()).div(b).add(T::one())
}

pub fn unsigned_align_to<T>(a: T, b: T) -> T
where
    T: num_traits::Unsigned + Copy,
{
    unsigned_rounded_up_div(a, b).mul(b)
}

#[test]
fn rounding_up() {
    assert_eq!(unsigned_rounded_up_div(5u32, 1), 5);
    assert_eq!(unsigned_rounded_up_div(5u32, 2), 3);
    assert_eq!(unsigned_rounded_up_div(5u32, 3), 2);
    assert_eq!(unsigned_rounded_up_div(5u32, 4), 2);
    assert_eq!(unsigned_rounded_up_div(5u32, 5), 1);
}

#[test]
fn alignment() {
    assert_eq!(unsigned_align_to(5u32, 8), 8);
    assert_eq!(unsigned_align_to(15u32, 8), 16);
}
