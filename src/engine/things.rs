// Generated from vendor/doomgeneric/info.h (mobjtype_t order). Do not reorder.

/// (display name, category) indexed by the engine's `mobjtype_t`.
pub const THING_TYPES: &[(&str, &str)] = &[
    ("Player", "player"),                     // MT_PLAYER
    ("Zombieman", "monster"),                 // MT_POSSESSED
    ("Shotgun Guy", "monster"),               // MT_SHOTGUY
    ("Arch-vile", "monster"),                 // MT_VILE
    ("Arch-vile Fire", "projectile"),         // MT_FIRE
    ("Revenant", "monster"),                  // MT_UNDEAD
    ("Revenant Missile", "projectile"),       // MT_TRACER
    ("Smoke", "other"),                       // MT_SMOKE
    ("Mancubus", "monster"),                  // MT_FATSO
    ("Mancubus Fireball", "projectile"),      // MT_FATSHOT
    ("Heavy Weapon Dude", "monster"),         // MT_CHAINGUY
    ("Imp", "monster"),                       // MT_TROOP
    ("Demon", "monster"),                     // MT_SERGEANT
    ("Spectre", "monster"),                   // MT_SHADOWS
    ("Cacodemon", "monster"),                 // MT_HEAD
    ("Baron of Hell", "monster"),             // MT_BRUISER
    ("Baron Fireball", "projectile"),         // MT_BRUISERSHOT
    ("Hell Knight", "monster"),               // MT_KNIGHT
    ("Lost Soul", "monster"),                 // MT_SKULL
    ("Spider Mastermind", "monster"),         // MT_SPIDER
    ("Arachnotron", "monster"),               // MT_BABY
    ("Cyberdemon", "monster"),                // MT_CYBORG
    ("Pain Elemental", "monster"),            // MT_PAIN
    ("Wolfenstein SS", "monster"),            // MT_WOLFSS
    ("Commander Keen", "monster"),            // MT_KEEN
    ("Icon of Sin", "monster"),               // MT_BOSSBRAIN
    ("Bossspit", "other"),                    // MT_BOSSSPIT
    ("Bosstarget", "other"),                  // MT_BOSSTARGET
    ("Demon Spawn Cube", "projectile"),       // MT_SPAWNSHOT
    ("Spawnfire", "other"),                   // MT_SPAWNFIRE
    ("Explosive Barrel", "barrel"),           // MT_BARREL
    ("Imp Fireball", "projectile"),           // MT_TROOPSHOT
    ("Cacodemon Fireball", "projectile"),     // MT_HEADSHOT
    ("Rocket", "projectile"),                 // MT_ROCKET
    ("Plasma Ball", "projectile"),            // MT_PLASMA
    ("BFG Ball", "projectile"),               // MT_BFG
    ("Arachnotron Plasma", "projectile"),     // MT_ARACHPLAZ
    ("Puff", "other"),                        // MT_PUFF
    ("Blood", "other"),                       // MT_BLOOD
    ("Tfog", "other"),                        // MT_TFOG
    ("Ifog", "other"),                        // MT_IFOG
    ("Teleportman", "other"),                 // MT_TELEPORTMAN
    ("Extrabfg", "other"),                    // MT_EXTRABFG
    ("Green Armor", "armor"),                 // MT_MISC0
    ("Blue Armor", "armor"),                  // MT_MISC1
    ("Health Bonus", "health"),               // MT_MISC2
    ("Armor Bonus", "armor"),                 // MT_MISC3
    ("Blue Keycard", "key"),                  // MT_MISC4
    ("Red Keycard", "key"),                   // MT_MISC5
    ("Yellow Keycard", "key"),                // MT_MISC6
    ("Yellow Skull Key", "key"),              // MT_MISC7
    ("Red Skull Key", "key"),                 // MT_MISC8
    ("Blue Skull Key", "key"),                // MT_MISC9
    ("Stimpack", "health"),                   // MT_MISC10
    ("Medikit", "health"),                    // MT_MISC11
    ("Soulsphere", "powerup"),                // MT_MISC12
    ("Invulnerability", "powerup"),           // MT_INV
    ("Berserk", "powerup"),                   // MT_MISC13
    ("Partial Invisibility", "powerup"),      // MT_INS
    ("Radiation Suit", "powerup"),            // MT_MISC14
    ("Computer Map", "powerup"),              // MT_MISC15
    ("Light Amplification Visor", "powerup"), // MT_MISC16
    ("Megasphere", "powerup"),                // MT_MEGA
    ("Clip", "ammo"),                         // MT_CLIP
    ("Box of Bullets", "ammo"),               // MT_MISC17
    ("Rocket", "ammo"),                       // MT_MISC18
    ("Box of Rockets", "ammo"),               // MT_MISC19
    ("Energy Cell", "ammo"),                  // MT_MISC20
    ("Energy Cell Pack", "ammo"),             // MT_MISC21
    ("Shotgun Shells", "ammo"),               // MT_MISC22
    ("Box of Shells", "ammo"),                // MT_MISC23
    ("Backpack", "ammo"),                     // MT_MISC24
    ("BFG 9000", "weapon"),                   // MT_MISC25
    ("Chaingun", "weapon"),                   // MT_CHAINGUN
    ("Chainsaw", "weapon"),                   // MT_MISC26
    ("Rocket Launcher", "weapon"),            // MT_MISC27
    ("Plasma Rifle", "weapon"),               // MT_MISC28
    ("Shotgun", "weapon"),                    // MT_SHOTGUN
    ("Super Shotgun", "weapon"),              // MT_SUPERSHOTGUN
    ("Misc29", "other"),                      // MT_MISC29
    ("Misc30", "other"),                      // MT_MISC30
    ("Misc31", "other"),                      // MT_MISC31
    ("Misc32", "other"),                      // MT_MISC32
    ("Misc33", "other"),                      // MT_MISC33
    ("Misc34", "other"),                      // MT_MISC34
    ("Misc35", "other"),                      // MT_MISC35
    ("Misc36", "other"),                      // MT_MISC36
    ("Misc37", "other"),                      // MT_MISC37
    ("Misc38", "other"),                      // MT_MISC38
    ("Misc39", "other"),                      // MT_MISC39
    ("Misc40", "other"),                      // MT_MISC40
    ("Misc41", "other"),                      // MT_MISC41
    ("Misc42", "other"),                      // MT_MISC42
    ("Misc43", "other"),                      // MT_MISC43
    ("Misc44", "other"),                      // MT_MISC44
    ("Misc45", "other"),                      // MT_MISC45
    ("Misc46", "other"),                      // MT_MISC46
    ("Misc47", "other"),                      // MT_MISC47
    ("Misc48", "other"),                      // MT_MISC48
    ("Misc49", "other"),                      // MT_MISC49
    ("Misc50", "other"),                      // MT_MISC50
    ("Misc51", "other"),                      // MT_MISC51
    ("Misc52", "other"),                      // MT_MISC52
    ("Misc53", "other"),                      // MT_MISC53
    ("Misc54", "other"),                      // MT_MISC54
    ("Misc55", "other"),                      // MT_MISC55
    ("Misc56", "other"),                      // MT_MISC56
    ("Misc57", "other"),                      // MT_MISC57
    ("Misc58", "other"),                      // MT_MISC58
    ("Misc59", "other"),                      // MT_MISC59
    ("Misc60", "other"),                      // MT_MISC60
    ("Misc61", "other"),                      // MT_MISC61
    ("Misc62", "other"),                      // MT_MISC62
    ("Misc63", "other"),                      // MT_MISC63
    ("Misc64", "other"),                      // MT_MISC64
    ("Misc65", "other"),                      // MT_MISC65
    ("Misc66", "other"),                      // MT_MISC66
    ("Misc67", "other"),                      // MT_MISC67
    ("Misc68", "other"),                      // MT_MISC68
    ("Misc69", "other"),                      // MT_MISC69
    ("Misc70", "other"),                      // MT_MISC70
    ("Misc71", "other"),                      // MT_MISC71
    ("Misc72", "other"),                      // MT_MISC72
    ("Misc73", "other"),                      // MT_MISC73
    ("Misc74", "other"),                      // MT_MISC74
    ("Misc75", "other"),                      // MT_MISC75
    ("Misc76", "other"),                      // MT_MISC76
    ("Misc77", "other"),                      // MT_MISC77
    ("Misc78", "other"),                      // MT_MISC78
    ("Misc79", "other"),                      // MT_MISC79
    ("Misc80", "other"),                      // MT_MISC80
    ("Misc81", "other"),                      // MT_MISC81
    ("Misc82", "other"),                      // MT_MISC82
    ("Misc83", "other"),                      // MT_MISC83
    ("Misc84", "other"),                      // MT_MISC84
    ("Misc85", "other"),                      // MT_MISC85
    ("Misc86", "other"),                      // MT_MISC86
];
