//! Users of the berth, from its own /etc/passwd.

#[derive(Debug, PartialEq)]
pub struct User {
    pub uid: u32,
    pub gid: u32,
    pub name: String,
    pub home: String,
    pub shell: String,
}

/// A user of the berth by name or uid.
pub fn lookup(user: &str) -> Option<User> {
    find(&std::fs::read_to_string("/etc/passwd").ok()?, user)
}

/// A user by name or uid in the text of a passwd file.
pub fn find(passwd: &str, user: &str) -> Option<User> {
    passwd.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        if f.len() < 7 || (f[0] != user && f[2] != user) {
            return None;
        }
        Some(User {
            name: f[0].into(),
            uid: f[2].parse().ok()?,
            gid: f[3].parse().ok()?,
            home: f[5].into(),
            shell: f[6].into(),
        })
    })
}

/// A group's id by name, from the berth's /etc/group.
pub fn group_id(name: &str) -> Option<libc::gid_t> {
    let group = std::fs::read_to_string("/etc/group").ok()?;
    group.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() == 4 && f[0] == name).then(|| f[2].parse().ok()).flatten()
    })
}

/// The supplementary groups of user from the berth's /etc/group.
pub fn groups(user: &User) -> Vec<libc::gid_t> {
    member_of(&std::fs::read_to_string("/etc/group").unwrap_or_default(), user)
}

/// The groups that list user as a member in the text of a group file, and its
/// own group.
pub fn member_of(group: &str, user: &User) -> Vec<libc::gid_t> {
    let mut gids = vec![user.gid];
    for l in group.lines() {
        let f: Vec<&str> = l.split(':').collect();
        if f.len() == 4
            && f[3].split(',').any(|m| m == user.name)
            && let Ok(gid) = f[2].parse()
            && !gids.contains(&gid)
        {
            gids.push(gid);
        }
    }
    gids
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/bash
nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin
broken:x:12
agent:x:1000:1000::/home/agent:/bin/bash
";

    #[test]
    fn users_are_found_by_name_or_uid() {
        let agent =
            User { uid: 1000, gid: 1000, name: "agent".into(), home: "/home/agent".into(), shell: "/bin/bash".into() };
        assert_eq!(find(PASSWD, "agent"), Some(agent));
        assert_eq!(find(PASSWD, "0").map(|u| u.name), Some("root".into()));
    }

    #[test]
    fn supplementary_groups_come_from_the_group_file() {
        let agent = find(PASSWD, "agent").unwrap();
        let group = "root:x:0:\nsudo:x:27:agent\ndocker:x:998:other,agent\nother:x:5:other\nbad:x\n";
        assert_eq!(member_of(group, &agent), [1000, 27, 998]);
    }

    #[test]
    fn unknown_or_malformed_users_are_none() {
        assert_eq!(find(PASSWD, "ghost"), None);
        assert_eq!(find(PASSWD, "broken"), None);
        assert_eq!(find(PASSWD, "12"), None);
    }
}
