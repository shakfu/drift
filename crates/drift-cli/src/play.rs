//! Interactive single-player loop: fly a trader through the living galaxy.
//!
//! This is a thin driver over the simulation's command pipeline — every player
//! action becomes a `Command` applied at a tick boundary, exactly as a networked
//! client would issue it. The loop is generic over its input/output streams so it
//! can be driven by a terminal or by a scripted test.

use std::io::{BufRead, Write};

use drift_core::{CommodityId, Money, Quantity, SystemId};
use drift_economy::{
    Command, Contract, ContractId, ContractKind, FutureSide, Loan, LoanId, PlayerId, Trader,
    TraderId, TraderLocation, World,
};
use drift_mods::Registry;

/// A parsed player action (not yet bound to a specific trader).
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Buy(CommodityId, Quantity),
    Sell(CommodityId, Quantity),
    Jump(SystemId),
    Wait(u64),
    Status,
    Map,
    /// List the delivery-contract board.
    Contracts,
    /// Take on an open contract by id.
    Accept(ContractId),
    /// Deliver a held contract at the current system for its reward.
    Fulfil(ContractId),
    /// List the player's outstanding loans.
    Loans,
    /// Borrow a principal against the current trader.
    Borrow(Money),
    /// Repay a loan by id; `None` repays as much as capital allows.
    Repay(LoanId, Option<Money>),
    /// Insure the current trader against loss.
    Insure,
    /// Open a futures position on a commodity.
    Future(CommodityId, Quantity, FutureSide),
    /// List all financial positions (loans, insurance, futures).
    Finance,
    Help,
    Quit,
}

/// Resolve a commodity from a player token: exact id, `core:`-prefixed id, or a
/// case-insensitive display-name match.
fn resolve_commodity(reg: &Registry, token: &str) -> Option<CommodityId> {
    reg.commodities()
        .find(|(cid, def)| {
            let id = reg.commodity_name(*cid);
            id == token
                || id.strip_prefix("core:") == Some(token)
                || def.name.eq_ignore_ascii_case(token)
        })
        .map(|(cid, _)| cid)
}

/// Resolve a system from a player token, the same way.
fn resolve_system(reg: &Registry, token: &str) -> Option<SystemId> {
    reg.systems()
        .find(|s| {
            let id = reg.system_name(s.id);
            id == token
                || id.strip_prefix("core:") == Some(token)
                || s.name.eq_ignore_ascii_case(token)
        })
        .map(|s| s.id)
}

/// Parse a positive quantity token.
fn parse_qty(t: &str) -> Result<u32, String> {
    t.parse::<u32>().map_err(|_| format!("`{t}` is not a number"))
}

/// Parse a contract id token, tolerating a leading `#` (as printed in the board).
fn parse_contract_id(t: &str) -> Result<ContractId, String> {
    let n = t.strip_prefix('#').unwrap_or(t);
    n.parse::<u64>()
        .map(ContractId)
        .map_err(|_| format!("`{t}` is not a contract id"))
}

/// Parse a loan id token, tolerating a leading `#`.
fn parse_loan_id(t: &str) -> Result<LoanId, String> {
    let n = t.strip_prefix('#').unwrap_or(t);
    n.parse::<u64>()
        .map(LoanId)
        .map_err(|_| format!("`{t}` is not a loan id"))
}

/// Parse a positive credit amount.
fn parse_money(t: &str) -> Result<Money, String> {
    let n: i64 = t.parse().map_err(|_| format!("`{t}` is not an amount"))?;
    if n <= 0 {
        return Err("amount must be positive".into());
    }
    Ok(n)
}

/// Parse a futures side token.
fn parse_side(t: &str) -> Result<FutureSide, String> {
    match t.to_ascii_lowercase().as_str() {
        "long" | "buy" => Ok(FutureSide::Long),
        "short" | "sell" => Ok(FutureSide::Short),
        _ => Err(format!("side must be long or short, not `{t}`")),
    }
}

/// Parse a line of input into an [`Action`], or a human-readable error.
pub fn parse_action(line: &str, reg: &Registry) -> Result<Action, String> {
    let mut it = line.split_whitespace();
    let Some(verb) = it.next() else {
        return Err("type a command (try `help`)".into());
    };
    let rest: Vec<&str> = it.collect();

    match verb.to_ascii_lowercase().as_str() {
        "buy" | "b" => match rest.as_slice() {
            [c, q] => {
                let cid = resolve_commodity(reg, c).ok_or(format!("no such commodity `{c}`"))?;
                Ok(Action::Buy(cid, parse_qty(q)?))
            }
            _ => Err("usage: buy <commodity> <qty>".into()),
        },
        "sell" => match rest.as_slice() {
            [c, q] => {
                let cid = resolve_commodity(reg, c).ok_or(format!("no such commodity `{c}`"))?;
                Ok(Action::Sell(cid, parse_qty(q)?))
            }
            _ => Err("usage: sell <commodity> <qty>".into()),
        },
        "jump" | "j" => match rest.as_slice() {
            [dest] => {
                let sid = resolve_system(reg, dest).ok_or(format!("no such system `{dest}`"))?;
                Ok(Action::Jump(sid))
            }
            _ => Err("usage: jump <system>".into()),
        },
        "wait" | "w" => {
            let n = match rest.first() {
                Some(t) => parse_qty(t)? as u64,
                None => 1,
            };
            Ok(Action::Wait(n))
        }
        "status" | "s" | "look" | "l" => Ok(Action::Status),
        "map" | "m" => Ok(Action::Map),
        "contracts" | "c" | "board" => Ok(Action::Contracts),
        "accept" | "take" => match rest.as_slice() {
            [id] => Ok(Action::Accept(parse_contract_id(id)?)),
            _ => Err("usage: accept <id>".into()),
        },
        "fulfil" | "fulfill" | "deliver" => match rest.as_slice() {
            [id] => Ok(Action::Fulfil(parse_contract_id(id)?)),
            _ => Err("usage: fulfil <id>".into()),
        },
        "loans" | "debt" => Ok(Action::Loans),
        "borrow" | "loan" => match rest.as_slice() {
            [amt] => Ok(Action::Borrow(parse_money(amt)?)),
            _ => Err("usage: borrow <amount>".into()),
        },
        "repay" => match rest.as_slice() {
            [id] => Ok(Action::Repay(parse_loan_id(id)?, None)),
            [id, amt] => Ok(Action::Repay(parse_loan_id(id)?, Some(parse_money(amt)?))),
            _ => Err("usage: repay <id> [amount]".into()),
        },
        "insure" | "insurance" => Ok(Action::Insure),
        "future" | "fut" => match rest.as_slice() {
            [side, c, q] => {
                let side = parse_side(side)?;
                let cid = resolve_commodity(reg, c).ok_or(format!("no such commodity `{c}`"))?;
                Ok(Action::Future(cid, parse_qty(q)?, side))
            }
            _ => Err("usage: future <long|short> <commodity> <qty>".into()),
        },
        "finance" | "positions" => Ok(Action::Finance),
        "help" | "h" | "?" => Ok(Action::Help),
        "quit" | "q" | "exit" => Ok(Action::Quit),
        other => Err(format!("unknown command `{other}` (try `help`)")),
    }
}

pub const HELP: &str = "\
Commands:
  buy  <commodity> <qty>   purchase goods at the local market
  sell <commodity> <qty>   sell goods from your hold
  jump <system>            travel to a connected system (risky if laden!)
  wait [n]                 let n ticks pass (default 1)
  status                   show your situation
  map                      list systems and danger
  contracts                list the delivery-contract board
  accept <id>              take on an open contract
  fulfil <id>              deliver a held contract here for its reward
  loans                    list your outstanding loans
  borrow <amount>          take a loan against your ship (at a station)
  repay <id> [amount]      repay a loan (as much as you can afford if no amount)
  insure                   insure your ship against loss (at a station)
  future <side> <c> <qty>  open a futures position (side = long or short)
  finance                  list your loans, insurance, and futures
  help                     this text
  quit                     leave the game";

fn find(world: &World, id: TraderId) -> Option<&Trader> {
    world.traders().iter().find(|t| t.id == id)
}

/// Mass of goods currently in a trader's hold.
fn hold_used(reg: &Registry, trader: &Trader) -> u32 {
    trader
        .cargo
        .iter()
        .map(|(c, q)| q * reg.commodity(*c).unit_mass)
        .sum()
}

/// Render the docked player's situation.
fn dashboard(reg: &Registry, world: &World, id: TraderId) -> String {
    let Some(t) = find(world, id) else {
        return "You have no ship.".into();
    };
    let TraderLocation::Docked(sys) = t.location else {
        return "In transit...".into();
    };
    let mut s = String::new();
    let sysdef = reg.system(sys);
    s += &format!("\n-- Tick {} --\n", world.tick_count().get());
    s += &format!(
        "At {} (danger {:.2})    Capital: {} cr\n",
        sysdef.name, sysdef.danger, t.capital
    );
    let cap = reg.ship(t.ship).cargo_capacity;
    if t.cargo.is_empty() {
        s += &format!("Hold: empty [0/{cap}]\n");
    } else {
        let items: Vec<String> = t
            .cargo
            .iter()
            .map(|(c, q)| format!("{} x{}", reg.commodity(*c).name, q))
            .collect();
        s += &format!("Hold: {} [{}/{}]\n", items.join(", "), hold_used(reg, t), cap);
    }
    let held: Vec<String> = world
        .contracts()
        .iter()
        .filter(|c| c.held_by() == Some(id))
        .map(|c| {
            format!(
                "#{} ({} -> {})",
                c.id.0,
                contract_task(reg, c),
                reg.system(c.destination).name
            )
        })
        .collect();
    if !held.is_empty() {
        s += &format!("Contracts held: {}\n", held.join(", "));
    }
    let debt: Money = world
        .loans()
        .iter()
        .filter(|l| l.borrower == id)
        .map(|l| l.outstanding)
        .sum();
    if debt > 0 {
        s += &format!("Debt: {debt} cr owed\n");
    }
    s += "Market:\n";
    let market = &world.markets()[sys.index()];
    for (cid, good) in market.goods.iter() {
        s += &format!(
            "  {:<12} price {:>6}   stock {:>5}\n",
            reg.commodity(*cid).name,
            good.price,
            good.stock
        );
    }
    let jumps: Vec<String> = sysdef
        .connections
        .iter()
        .map(|c| format!("{}(d{:.2})", reg.system(*c).name, reg.system(*c).danger))
        .collect();
    s += &format!("Jumps: {}\n", jumps.join("  "));
    s
}

/// A one-line description of what a contract asks for (destination shown
/// separately): a delivery's cargo, a bounty's kill progress, or a courier parcel.
fn contract_task(reg: &Registry, c: &Contract) -> String {
    match c.kind {
        ContractKind::Delivery { commodity, quantity } => {
            format!("{quantity} {}", reg.commodity(commodity).name)
        }
        ContractKind::Courier => "courier parcel".to_string(),
        ContractKind::Bounty { target, progress } => format!("bounty {progress}/{target} kills"),
    }
}

/// Render the contract board: id, task, destination, reward, time left, and
/// whether each contract is open, held by this trader, or taken by another.
fn contracts_text(reg: &Registry, world: &World, id: TraderId) -> String {
    let board = world.contracts();
    if board.is_empty() {
        return "\nNo contracts on the board.\n".into();
    }
    let now = world.tick_count().get();
    let mut s = String::from("\nContracts:\n");
    for c in board {
        let left = c.deadline.get().saturating_sub(now);
        let status = match c.holder() {
            None => "open",
            Some((_, tid)) if tid == id => "yours",
            Some(_) => "taken",
        };
        s += &format!(
            "  #{:<3} {:<16} -> {:<10} {:>8} cr  {:>4}t  [{}]\n",
            c.id.0,
            contract_task(reg, c),
            reg.system(c.destination).name,
            c.reward,
            left,
            status,
        );
    }
    s += "(accept <id> to take one; complete it at its destination, then fulfil <id>)\n";
    s
}

/// Render the player's outstanding loans against `id`.
fn loans_text(world: &World, id: TraderId) -> String {
    let mine: Vec<&Loan> = world.loans().iter().filter(|l| l.borrower == id).collect();
    if mine.is_empty() {
        return "\nNo outstanding loans.\n".into();
    }
    let now = world.tick_count().get();
    let mut s = String::from("\nLoans:\n");
    for l in mine {
        s += &format!(
            "  #{:<3} owed {:>8} cr   due in {:>4}t\n",
            l.id.0,
            l.outstanding,
            l.due.get().saturating_sub(now)
        );
    }
    s += "(borrow <amount>; repay <id> [amount])\n";
    s
}

/// Render the player's full financial position: loans, insurance, and futures.
fn finance_text(reg: &Registry, world: &World, id: TraderId) -> String {
    let now = world.tick_count().get();
    let mut s = String::from("\nFinancial positions:\n");

    let loans: Vec<&Loan> = world.loans().iter().filter(|l| l.borrower == id).collect();
    if loans.is_empty() {
        s += "  Loans:     none\n";
    } else {
        for l in loans {
            s += &format!(
                "  Loan #{}   owed {} cr, due in {}t\n",
                l.id.0,
                l.outstanding,
                l.due.get().saturating_sub(now)
            );
        }
    }

    match world.policies().iter().find(|p| p.insured == id) {
        Some(p) => {
            s += &format!(
                "  Insurance: covered for {} cr (lapses in {}t)\n",
                p.payout,
                p.expiry.get().saturating_sub(now)
            )
        }
        None => s += "  Insurance: none\n",
    }

    let futures: Vec<_> = world.futures().iter().filter(|f| f.holder == id).collect();
    if futures.is_empty() {
        s += "  Futures:   none\n";
    } else {
        for f in futures {
            s += &format!(
                "  Future:    {:?} {} {} @ {} cr, matures in {}t\n",
                f.side,
                f.quantity,
                reg.commodity(f.commodity).name,
                f.strike,
                f.maturity.get().saturating_sub(now)
            );
        }
    }
    s
}

fn map_text(reg: &Registry) -> String {
    let mut s = String::from("\nSystems:\n");
    for sys in reg.systems() {
        let conns: Vec<&str> = sys.connections.iter().map(|c| reg.system(*c).name.as_str()).collect();
        s += &format!(
            "  {:<10} danger {:.2}  -> {}\n",
            sys.name,
            sys.danger,
            conns.join(", ")
        );
    }
    s
}

/// After a jump departs, advance ticks until the player is docked again,
/// narrating anything that happened in transit (pirate fights, destruction).
fn resolve_transit(reg: &Registry, world: &mut World, id: TraderId) -> Vec<String> {
    let mut log = Vec::new();
    let mut destroyed = false;
    for _ in 0..100_000 {
        let loc = find(world, id).map(|t| t.location.clone());
        match loc {
            Some(TraderLocation::Docked(sys)) => {
                let name = &reg.system(sys).name;
                log.push(if destroyed {
                    format!("Respawned at {name}.")
                } else {
                    format!("Arrived at {name}.")
                });
                break;
            }
            Some(TraderLocation::Destroyed { .. }) => {
                if !destroyed {
                    log.push("You were ambushed and destroyed! Your cargo is lost.".into());
                    destroyed = true;
                }
                world.tick();
            }
            Some(TraderLocation::InTransit { .. }) => {
                let before = find(world, id).map(|t| t.capital).unwrap_or(0);
                world.tick();
                let after = find(world, id).map(|t| t.capital).unwrap_or(0);
                if after > before {
                    log.push(format!(
                        "Fought off pirates and claimed {} cr in bounties!",
                        after - before
                    ));
                }
            }
            None => break,
        }
    }
    log
}

/// Report the outcome of a just-applied command (from the world's error channel).
fn report_command(world: &World, out: &mut impl Write, success: &str) -> std::io::Result<()> {
    match world.last_command_errors().first() {
        Some(e) => writeln!(out, "  rejected: {e}"),
        None => writeln!(out, "  {success}"),
    }
}

/// Run the interactive loop until EOF or `quit`. Generic over the streams so it is
/// driven by a terminal in the binary and by a `Cursor` in tests.
pub fn run_repl<R: BufRead, W: Write>(
    reg: &Registry,
    world: &mut World,
    player: PlayerId,
    id: TraderId,
    mut input: R,
    mut out: W,
) -> std::io::Result<()> {
    writeln!(out, "{HELP}")?;
    loop {
        // The player is always docked at the prompt.
        if find(world, id).is_none() {
            writeln!(out, "Your ship is gone. Game over.")?;
            break;
        }
        write!(out, "{}", dashboard(reg, world, id))?;
        write!(out, "> ")?;
        out.flush()?;

        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            break; // EOF
        }
        if line.trim().is_empty() {
            continue;
        }

        match parse_action(&line, reg) {
            Err(msg) => writeln!(out, "  {msg}")?,
            Ok(Action::Quit) => break,
            Ok(Action::Help) => writeln!(out, "{HELP}")?,
            Ok(Action::Status) => {} // dashboard reprints next loop
            Ok(Action::Map) => write!(out, "{}", map_text(reg))?,
            Ok(Action::Contracts) => write!(out, "{}", contracts_text(reg, world, id))?,
            Ok(Action::Accept(cid)) => {
                world.queue_command(Command::AcceptContract { player, trader: id, contract: cid });
                world.tick();
                report_command(world, &mut out, &format!("Accepted contract #{}.", cid.0))?;
            }
            Ok(Action::Fulfil(cid)) => {
                // Capture the reward before the tick, since a fulfilled contract is
                // removed from the board.
                let reward = world.contracts().iter().find(|c| c.id == cid).map(|c| c.reward);
                world.queue_command(Command::FulfillContract { player, trader: id, contract: cid });
                world.tick();
                match (world.last_command_errors().first(), reward) {
                    (Some(e), _) => writeln!(out, "  rejected: {e}")?,
                    (None, Some(r)) => writeln!(out, "  Completed contract #{}! +{} cr", cid.0, r)?,
                    (None, None) => writeln!(out, "  Completed contract #{}!", cid.0)?,
                }
            }
            Ok(Action::Loans) => write!(out, "{}", loans_text(world, id))?,
            Ok(Action::Finance) => write!(out, "{}", finance_text(reg, world, id))?,
            Ok(Action::Insure) => {
                world.queue_command(Command::BuyInsurance { player, trader: id });
                world.tick();
                report_command(world, &mut out, "Insured your ship.")?;
            }
            Ok(Action::Future(commodity, qty, side)) => {
                world.queue_command(Command::OpenFuture { player, trader: id, commodity, qty, side });
                world.tick();
                report_command(
                    world,
                    &mut out,
                    &format!("Opened a {:?} future on {} {}.", side, qty, reg.commodity(commodity).name),
                )?;
            }
            Ok(Action::Borrow(amount)) => {
                world.queue_command(Command::TakeLoan { player, trader: id, principal: amount });
                world.tick();
                report_command(world, &mut out, &format!("Borrowed {amount} cr."))?;
            }
            Ok(Action::Repay(loan, amount)) => {
                // No amount given: repay as much of the balance as capital allows.
                let owed = world.loans().iter().find(|l| l.id == loan).map(|l| l.outstanding);
                let cap = find(world, id).map(|t| t.capital).unwrap_or(0);
                let pay = amount.unwrap_or_else(|| owed.unwrap_or(0).min(cap)).max(1);
                world.queue_command(Command::RepayLoan { player, trader: id, loan, amount: pay });
                world.tick();
                report_command(world, &mut out, &format!("Repaid {pay} cr on loan #{}.", loan.0))?;
            }
            Ok(Action::Wait(n)) => {
                world.run(n.max(1));
                writeln!(out, "  {n} tick(s) passed.")?;
            }
            Ok(Action::Buy(c, q)) => {
                world.queue_command(Command::Buy { player, trader: id, commodity: c, qty: q });
                world.tick();
                report_command(world, &mut out, &format!("Bought {} {}.", q, reg.commodity(c).name))?;
            }
            Ok(Action::Sell(c, q)) => {
                world.queue_command(Command::Sell { player, trader: id, commodity: c, qty: q });
                world.tick();
                report_command(world, &mut out, &format!("Sold {} {}.", q, reg.commodity(c).name))?;
            }
            Ok(Action::Jump(dest)) => {
                world.queue_command(Command::Jump { player, trader: id, dest });
                world.tick();
                if let Some(e) = world.last_command_errors().first() {
                    writeln!(out, "  rejected: {e}")?;
                } else {
                    writeln!(out, "  Departing for {}...", reg.system(dest).name)?;
                    for msg in resolve_transit(reg, world, id) {
                        writeln!(out, "  {msg}")?;
                    }
                }
            }
        }
    }
    writeln!(out, "Fair skies, commander.")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::io::Cursor;
    use std::path::PathBuf;
    use std::sync::Arc;

    use drift_data::{
        CommodityAmount, CommodityDef, ContractConfig, FutureConfig, InsuranceConfig, LoanConfig,
        ProductionRecipe, ScenarioDef, ShipDef, SystemDef, TraderSpawn,
    };
    use drift_economy::builtin_pricing;
    use drift_mods::{link, load_and_link, MergedContent};

    use super::*;

    fn reg() -> Arc<Registry> {
        let pricing: HashSet<String> = builtin_pricing().names().map(String::from).collect();
        let mods = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mods");
        Arc::new(load_and_link(&mods, &pricing).expect("core mods link"))
    }

    /// A sandbox: no NPC traders, no piracy — deterministic and quiet.
    fn sandbox() -> ScenarioDef {
        ScenarioDef {
            name: "sandbox".into(),
            seed: 1,
            ticks: 0,
            traders: TraderSpawn { count: 0, ship: "core:cobra_mk3".into(), starting_capital: 0 },
            piracy: None,
            risk_aversion: 0.0,
            escort: None,
            navy: None,
            contract: None,
            loan: None,
            insurance: None,
            future: None,
        }
    }

    #[test]
    fn parses_actions_and_resolves_names() {
        let r = reg();
        let food = r.commodity_id("core:food").unwrap();
        let leesti = r.system_id("core:leesti").unwrap();
        assert_eq!(parse_action("buy food 10", &r), Ok(Action::Buy(food, 10)));
        assert_eq!(parse_action("b Food 3", &r), Ok(Action::Buy(food, 3)));
        assert_eq!(parse_action("sell core:food 5", &r), Ok(Action::Sell(food, 5)));
        assert_eq!(parse_action("jump leesti", &r), Ok(Action::Jump(leesti)));
        assert_eq!(parse_action("wait 4", &r), Ok(Action::Wait(4)));
        assert_eq!(parse_action("wait", &r), Ok(Action::Wait(1)));
        assert_eq!(parse_action("s", &r), Ok(Action::Status));
        assert_eq!(parse_action("map", &r), Ok(Action::Map));
        assert_eq!(parse_action("quit", &r), Ok(Action::Quit));
        // Errors:
        assert!(parse_action("buy nope 1", &r).is_err());
        assert!(parse_action("buy food xx", &r).is_err());
        assert!(parse_action("buy food", &r).is_err());
        assert!(parse_action("frobnicate", &r).is_err());
    }

    #[test]
    fn parses_contract_actions() {
        let r = reg();
        assert_eq!(parse_action("contracts", &r), Ok(Action::Contracts));
        assert_eq!(parse_action("c", &r), Ok(Action::Contracts));
        assert_eq!(parse_action("accept 7", &r), Ok(Action::Accept(ContractId(7))));
        assert_eq!(parse_action("accept #7", &r), Ok(Action::Accept(ContractId(7))));
        assert_eq!(parse_action("fulfil 3", &r), Ok(Action::Fulfil(ContractId(3))));
        assert_eq!(parse_action("fulfill 3", &r), Ok(Action::Fulfil(ContractId(3))));
        // Errors:
        assert!(parse_action("accept", &r).is_err());
        assert!(parse_action("accept xx", &r).is_err());
        assert!(parse_action("fulfil", &r).is_err());
    }

    #[test]
    fn parses_finance_actions() {
        let r = reg();
        assert_eq!(parse_action("loans", &r), Ok(Action::Loans));
        assert_eq!(parse_action("borrow 500", &r), Ok(Action::Borrow(500)));
        assert_eq!(parse_action("loan 1000", &r), Ok(Action::Borrow(1000)));
        assert_eq!(parse_action("repay 2", &r), Ok(Action::Repay(LoanId(2), None)));
        assert_eq!(parse_action("repay #2 300", &r), Ok(Action::Repay(LoanId(2), Some(300))));
        // Errors:
        assert!(parse_action("borrow", &r).is_err());
        assert!(parse_action("borrow -5", &r).is_err());
        assert!(parse_action("repay", &r).is_err());
        assert!(parse_action("repay xx", &r).is_err());
    }

    #[test]
    fn parses_insurance_and_future_actions() {
        let r = reg();
        let food = r.commodity_id("core:food").unwrap();
        assert_eq!(parse_action("insure", &r), Ok(Action::Insure));
        assert_eq!(parse_action("finance", &r), Ok(Action::Finance));
        assert_eq!(
            parse_action("future long food 5", &r),
            Ok(Action::Future(food, 5, FutureSide::Long))
        );
        assert_eq!(
            parse_action("fut short food 3", &r),
            Ok(Action::Future(food, 3, FutureSide::Short))
        );
        // Errors:
        assert!(parse_action("future sideways food 5", &r).is_err());
        assert!(parse_action("future long food", &r).is_err());
    }

    /// A sandbox offering insurance and a futures market over the core galaxy.
    fn finance_sandbox() -> ScenarioDef {
        ScenarioDef {
            name: "finance".into(),
            seed: 1,
            ticks: 0,
            traders: TraderSpawn { count: 0, ship: "core:cobra_mk3".into(), starting_capital: 0 },
            piracy: None,
            risk_aversion: 0.0,
            escort: None,
            navy: None,
            contract: None,
            loan: None,
            insurance: Some(InsuranceConfig { premium: 500, payout: 4000, term_ticks: 1000 }),
            future: Some(FutureConfig {
                fee: 100,
                term_ticks: 200,
                max_quantity: 50,
                max_futures: 3,
            }),
        }
    }

    #[test]
    fn scripted_session_insures_and_opens_a_future() {
        let r = reg();
        let pricing = builtin_pricing();
        let mut world = World::new(r.clone(), &finance_sandbox(), 1, &pricing).unwrap();
        let (player, id) = spawn_player(&r, &mut world, 5000);

        let input = Cursor::new(b"insure\nfuture long food 5\nfinance\nquit\n".to_vec());
        let mut out: Vec<u8> = Vec::new();
        run_repl(&r, &mut world, player, id, input, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();

        assert!(text.contains("Insured your ship"), "insure confirmed:\n{text}");
        assert!(text.contains("Opened a Long future on 5 Food"), "future confirmed:\n{text}");
        assert!(text.contains("covered for 4000"), "finance lists the policy:\n{text}");

        assert_eq!(world.policies().len(), 1, "the policy is active");
        assert_eq!(world.futures().len(), 1, "the position is open");
    }

    /// A lending-enabled sandbox over the core galaxy (interest-free for a clean
    /// principal check).
    fn loan_sandbox() -> ScenarioDef {
        ScenarioDef {
            name: "loans".into(),
            seed: 1,
            ticks: 0,
            traders: TraderSpawn { count: 0, ship: "core:cobra_mk3".into(), starting_capital: 0 },
            piracy: None,
            risk_aversion: 0.0,
            escort: None,
            navy: None,
            contract: None,
            loan: Some(LoanConfig {
                rate_bps: 0,
                accrual_interval: 100,
                term_ticks: 1000,
                max_principal: 10_000,
                max_loans: 2,
            }),
            insurance: None,
            future: None,
        }
    }

    #[test]
    fn scripted_session_borrows_and_repays() {
        let r = reg();
        let pricing = builtin_pricing();
        let mut world = World::new(r.clone(), &loan_sandbox(), 1, &pricing).unwrap();
        let (player, id) = spawn_player(&r, &mut world, 1000);

        // Borrow 5000, list the loan, then repay it in full (no amount = as much
        // as capital allows).
        let input = Cursor::new(b"borrow 5000\nloans\nrepay 0\nquit\n".to_vec());
        let mut out: Vec<u8> = Vec::new();
        run_repl(&r, &mut world, player, id, input, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();

        assert!(text.contains("Borrowed 5000 cr"), "borrow confirmed:\n{text}");
        assert!(text.contains("#0"), "the loan is listed:\n{text}");
        assert!(text.contains("Repaid 5000 cr on loan #0"), "repay confirmed:\n{text}");

        assert!(world.loans().is_empty(), "the loan is fully repaid and closed");
        let t = world.traders().iter().find(|t| t.id == id).unwrap();
        assert_eq!(t.capital, 1000, "capital returns to the pre-loan level");
    }

    /// A minimal two-system galaxy: Aport grows food, Bport eats it (so Bport
    /// develops a shortage), joined by a one-tick jump. Enough to drive a contract
    /// end-to-end deterministically.
    fn two_system_reg() -> Arc<Registry> {
        let pricing: HashSet<String> = builtin_pricing().names().map(String::from).collect();
        let merged = MergedContent {
            scripts: vec![],
            commodities: vec![CommodityDef {
                id: "t:food".into(),
                name: "Food".into(),
                base_price: 100,
                unit_mass: 1,
                elasticity: 1.0,
                category: "food".into(),
            }],
            recipes: vec![
                ProductionRecipe {
                    id: "t:grow".into(),
                    inputs: vec![],
                    outputs: vec![CommodityAmount { commodity: "t:food".into(), qty: 10 }],
                    rate: 1,
                    elasticity: 0.0,
                },
                ProductionRecipe {
                    id: "t:eat".into(),
                    inputs: vec![CommodityAmount { commodity: "t:food".into(), qty: 10 }],
                    outputs: vec![],
                    rate: 1,
                    elasticity: 0.0,
                },
            ],
            systems: vec![
                SystemDef {
                    id: "t:a".into(),
                    name: "Aport".into(),
                    position: [0.0, 0.0],
                    industries: vec!["t:grow".into()],
                    connections: vec!["t:b".into()],
                    initial_stock: vec![CommodityAmount { commodity: "t:food".into(), qty: 500 }],
                    pricing: "supply_demand_v1".into(),
                    danger: 0.0,
                },
                SystemDef {
                    id: "t:b".into(),
                    name: "Bport".into(),
                    position: [1.0, 0.0],
                    industries: vec!["t:eat".into()],
                    connections: vec!["t:a".into()],
                    initial_stock: vec![CommodityAmount { commodity: "t:food".into(), qty: 500 }],
                    pricing: "supply_demand_v1".into(),
                    danger: 0.0,
                },
            ],
            ships: vec![ShipDef {
                id: "t:freighter".into(),
                name: "Freighter".into(),
                cargo_capacity: 1000,
                jump_speed: 100.0,
                hull: 100,
                max_speed: 100.0,
                combat: None,
                visual: None,
            }],
        };
        Arc::new(link(merged, &pricing).unwrap())
    }

    fn contract_scenario() -> ScenarioDef {
        ScenarioDef {
            name: "contracts".into(),
            seed: 1,
            ticks: 0,
            traders: TraderSpawn { count: 0, ship: "t:freighter".into(), starting_capital: 0 },
            piracy: None,
            risk_aversion: 0.0,
            escort: None,
            navy: None,
            contract: Some(ContractConfig {
                max_open: 4,
                generation_interval: 5,
                deadline_ticks: 1000,
                reward_factor: 1.5,
                min_shortfall: 10,
                max_quantity: 20,
                // Delivery-only for a deterministic single-jump test.
                bounty_target: 0,
                bounty_reward: 0,
                courier_reward: 0,
            }),
            loan: None,
            insurance: None,
            future: None,
        }
    }

    #[test]
    fn scripted_session_accepts_and_fulfills_a_contract() {
        let r = two_system_reg();
        let pricing = builtin_pricing();
        let mut world = World::new(r.clone(), &contract_scenario(), 1, &pricing).unwrap();

        // Let a shortage build at Bport so a contract is posted.
        world.run(20);
        let contract = world
            .contracts()
            .iter()
            .find(|c| c.is_open())
            .expect("an open contract")
            .clone();
        let cid = contract.id.0;
        let need = contract.cargo().map(|(_, q)| q).expect("a delivery contract");
        let reward = contract.reward;
        assert_eq!(contract.destination, r.system_id("t:b").unwrap());

        // Spawn the player at Aport (the surplus source), with capital to buy.
        let player = PlayerId(0);
        let ship = r.ship_id("t:freighter").unwrap();
        let aport = r.system_id("t:a").unwrap();
        world.queue_command(Command::Spawn { player, ship, at: aport, capital: 1_000_000 });
        world.tick();
        let id = world.traders().iter().find(|t| t.is_player()).unwrap().id;

        // List the board, take the contract, load the cargo, run it to Bport, deliver.
        let script =
            format!("contracts\naccept {cid}\nbuy food {need}\njump bport\nfulfil {cid}\nquit\n");
        let input = Cursor::new(script.into_bytes());
        let mut out: Vec<u8> = Vec::new();
        run_repl(&r, &mut world, player, id, input, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();

        assert!(text.contains(&format!("#{cid}")), "the board should list the contract:\n{text}");
        assert!(text.contains(&format!("Accepted contract #{cid}")), "accept confirmed:\n{text}");
        assert!(text.contains(&format!("Completed contract #{cid}")), "delivery confirmed:\n{text}");
        assert!(text.contains(&format!("+{reward} cr")), "reward shown:\n{text}");

        // The world reflects it: the cargo is gone and the contract left the board.
        let food = r.commodity_id("t:food").unwrap();
        let t = world.traders().iter().find(|t| t.id == id).unwrap();
        assert_eq!(t.cargo.get(&food).copied().unwrap_or(0), 0, "delivered cargo is consumed");
        assert!(world.contracts().iter().all(|c| c.id.0 != cid), "the contract left the board");
    }

    #[test]
    fn fulfilling_from_the_wrong_place_is_rejected() {
        let r = two_system_reg();
        let pricing = builtin_pricing();
        let mut world = World::new(r.clone(), &contract_scenario(), 1, &pricing).unwrap();
        world.run(20);
        let cid = world.contracts().iter().find(|c| c.is_open()).expect("open contract").id.0;

        let player = PlayerId(0);
        let ship = r.ship_id("t:freighter").unwrap();
        let aport = r.system_id("t:a").unwrap();
        world.queue_command(Command::Spawn { player, ship, at: aport, capital: 1_000_000 });
        world.tick();
        let id = world.traders().iter().find(|t| t.is_player()).unwrap().id;

        // Accept, then try to deliver at the source (Aport), not the destination.
        let script = format!("accept {cid}\nfulfil {cid}\nquit\n");
        let input = Cursor::new(script.into_bytes());
        let mut out: Vec<u8> = Vec::new();
        run_repl(&r, &mut world, player, id, input, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("rejected:"), "delivery from the wrong place should be rejected:\n{text}");
    }

    fn spawn_player(r: &Registry, world: &mut World, capital: i64) -> (PlayerId, TraderId) {
        let player = PlayerId(0);
        let ship = r.ship_id("core:cobra_mk3").unwrap();
        let lave = r.system_id("core:lave").unwrap();
        world.queue_command(Command::Spawn { player, ship, at: lave, capital });
        world.tick();
        let id = world.traders().iter().find(|t| t.is_player()).unwrap().id;
        (player, id)
    }

    #[test]
    fn scripted_session_buys_and_sells() {
        let r = reg();
        let pricing = builtin_pricing();
        let mut world = World::new(r.clone(), &sandbox(), 1, &pricing).unwrap();
        let (player, id) = spawn_player(&r, &mut world, 5000);

        let input = Cursor::new(b"buy food 10\nsell food 4\nquit\n".to_vec());
        let mut out: Vec<u8> = Vec::new();
        run_repl(&r, &mut world, player, id, input, &mut out).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("Bought 10 Food"), "output:\n{text}");
        assert!(text.contains("Sold 4 Food"));
        assert!(text.contains("Fair skies"));
        // Net cargo: 10 bought - 4 sold = 6.
        let food = r.commodity_id("core:food").unwrap();
        let t = world.traders().iter().find(|t| t.id == id).unwrap();
        assert_eq!(t.cargo.get(&food).copied(), Some(6));
    }

    #[test]
    fn scripted_jump_moves_the_player() {
        let r = reg();
        let pricing = builtin_pricing();
        let mut world = World::new(r.clone(), &sandbox(), 1, &pricing).unwrap();
        let (player, id) = spawn_player(&r, &mut world, 5000);

        let input = Cursor::new(b"jump leesti\nquit\n".to_vec());
        let mut out: Vec<u8> = Vec::new();
        run_repl(&r, &mut world, player, id, input, &mut out).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("Departing for Leesti"), "output:\n{text}");
        assert!(text.contains("Arrived at Leesti"));
        let leesti = r.system_id("core:leesti").unwrap();
        let t = world.traders().iter().find(|t| t.id == id).unwrap();
        assert_eq!(t.location, TraderLocation::Docked(leesti));
    }

    #[test]
    fn rejected_command_is_reported_to_the_player() {
        let r = reg();
        let pricing = builtin_pricing();
        let mut world = World::new(r.clone(), &sandbox(), 1, &pricing).unwrap();
        let (player, id) = spawn_player(&r, &mut world, 100);

        // Cannot afford 100000 food, and cannot jump to an unconnected system.
        let input = Cursor::new(b"buy food 100000\njump tionisla\nquit\n".to_vec());
        let mut out: Vec<u8> = Vec::new();
        run_repl(&r, &mut world, player, id, input, &mut out).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.matches("rejected:").count(), 2, "both should be rejected:\n{text}");
    }
}
