use super::merge::{IsReachable, Merge, Merger};
use super::{Branching, DecTree, TreeTail};
use derive_new::new;
use lumina_parser::pat::Bound;
use lumina_typesystem::Bitsize;
use std::cmp::Ordering;
use std::fmt;
use tracing::warn;

#[derive(Debug, Clone, Copy, PartialEq, Eq, new)]
pub struct Range {
    pub con: Constraints,
    pub start: i128,
    pub end: i128,
}

impl Range {
    pub fn full(&self) -> Self {
        Range { con: self.con, start: self.con.max, end: self.con.max }
    }

    pub fn is_full(&self) -> bool {
        self.con.min == self.start && self.con.max == self.end
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Constraints {
    pub min: i128,
    pub max: i128,
}

impl Constraints {
    pub fn to_range(self) -> Range {
        Range { start: self.min, end: self.max, con: self }
    }
}

pub fn constraints_from_bitsize(signed: bool, bitsize: Bitsize) -> Constraints {
    let max = match signed {
        true => (1 << (bitsize.0 - 1) as i128) - 1,
        false => (1 << bitsize.0 as i128) - 1,
    };

    let min = signed.then(|| -max - 1).unwrap_or(0);

    Constraints { min, max }
}

impl<'a, 's, Tail: std::fmt::Display + Clone + PartialEq, M: Merge<'s, Tail>>
    Merger<'a, 's, Tail, M>
{
    pub fn merge_int_bounds(
        self,
        signed: bool,
        bitsize: Bitsize,
        next: &mut Branching<Range, Tail>,
        bounds: &[Bound; 2],
    ) -> IsReachable {
        let con = constraints_from_bitsize(signed, bitsize);

        for (range, _) in &next.branches {
            if range.con != con {
                warn!("reporting reachability as true due to poison");
                return true;
            }
        }

        let start = match bounds[0] {
            Bound::Excess => con.min,
            Bound::Neg(n) => -(n as i128),
            Bound::Pos(n) => n as i128,
        };

        let end = match bounds[1] {
            Bound::Excess => con.max,
            Bound::Neg(n) => -(n as i128),
            Bound::Pos(n) => n as i128,
        };

        if start > end {
            // TODO: I think we've forgotten to check for this in the tcheck pass
            //
            // so let's panic for now until we remember to solve that.
            panic!("start cannot be higher than end");
        }

        self.merge_int(next, start, end)
    }

    pub fn merge_int(
        self,
        next: &mut Branching<Range, Tail>,
        start: i128,
        end: i128,
    ) -> IsReachable {
        let reachable = self.merge_int_at(0, next, start, end);
        Self::cleanup_edges_if_at_end(next);
        reachable
    }

    // If the last part of this tree is the int and the tails are identical, we can merge them.
    //
    // This is mainly to make snapshot testing prettier.
    fn cleanup_edges_if_at_end(ints: &mut Branching<Range, Tail>) {
        let mut i = 1;
        loop {
            let Ok([(range, left), (rrange, right)]) = ints.branches.get_many_mut([i - 1, i])
            else {
                break;
            };

            match (left, right) {
                (DecTree::End(left), DecTree::End(right)) => match (left, right) {
                    (
                        TreeTail::Reached(ltable, lexcess, ltail),
                        TreeTail::Reached(rtable, rexcess, rtail),
                    ) if ltable == rtable && ltail == rtail => {
                        assert_eq!(lexcess.len(), rexcess.len());
                        range.end = rrange.end;
                        ints.branches.remove(i);
                    }
                    (TreeTail::Unreached(lexcess), TreeTail::Unreached(rexcess)) => {
                        lexcess.append(rexcess);
                        debug_assert!(range.end < rrange.end);
                        range.end = rrange.end;
                        ints.branches.remove(i);
                    }
                    _ => {
                        i += 1;
                    }
                },
                _ => break,
            }
        }
    }

    fn merge_int_at(
        mut self,
        mut i: usize,
        ints: &mut Branching<Range, Tail>,
        start: i128,
        end: i128,
    ) -> IsReachable {
        let (range, tree) = ints
            .branches
            .get_mut(i)
            .expect("complete int not generated from type");

        assert!(start <= end);

        let excluded = end < range.start || start > range.end;
        if excluded {
            return self.merge_int_at(i + 1, ints, start, end);
        }

        let mut reachable = false;

        match start.cmp(&range.start) {
            Ordering::Less => unreachable!("would've already been merged by now?"),
            Ordering::Equal => {}
            Ordering::Greater => {
                // we want to keep the branches sorted. So; if there's anything on the left-side
                // that we shouldn't merge with then we split that out first and continue with the rest.
                let untouched_left = Range::new(range.con, range.start, start - 1);
                let untouched_next = tree.clone();
                ints.branches.insert(i, (untouched_left, untouched_next));

                i += 1;
                ints.branches[i].0.start = start;
            }
        }

        let (range, tree) = &mut ints.branches[i];

        match end.cmp(&range.end) {
            Ordering::Less => {
                // ditto but for the right side
                let untouched_right = Range::new(range.con, end + 1, range.end);
                let untouched_next = tree.clone();
                ints.branches
                    .insert(i + 1, (untouched_right, untouched_next));

                ints.branches[i].0.end = end;
            }
            Ordering::Equal => {}
            Ordering::Greater => {
                let start = range.end + 1;
                reachable |= self.fork().merge_int_at(i + 1, ints, start, end);
            }
        }

        let (_, tree) = &mut ints.branches[i];
        reachable | self.next(tree)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constraints() {
        let cons = constraints_from_bitsize(true, Bitsize(64));
        assert_eq!(cons.max, i64::MAX as i128);
        assert_eq!(cons.min, i64::MIN as i128);

        let cons = constraints_from_bitsize(false, Bitsize(64));
        assert_eq!(cons.max, u64::MAX as i128);
        assert_eq!(cons.min, u64::MIN as i128);
    }
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.start == self.end {
            return self.start.fmt(f);
        }

        if self.start != self.con.min {
            self.start.fmt(f)?;
        }

        write!(f, "..")?;

        if self.end != self.con.max {
            self.end.fmt(f)?;
        }

        Ok(())
    }
}
