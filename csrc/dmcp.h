// C API between the doom-mcp Rust code and the doomgeneric engine.
// Mirrored by #[repr(C)] structs in src/doom/ffi.rs; keep them in sync.
#ifndef DMCP_H
#define DMCP_H

#include <stdint.h>

typedef struct
{
    int up, down, left, right;
    int strafeleft, straferight;
    int fire, use, speed;
} dmcp_keys_t;

typedef struct
{
    int gamemode;
    int gamestate;
    int menuactive;
    int paused;
    int demoplayback;
    int automapactive;
    int episode;
    int map;
    int skill;
    int gametic;
    int leveltime;
    int totalkills;
    int totalitems;
    int totalsecrets;

    int in_level;
    int playerstate;
    int health;
    int armor;
    int armortype;
    int readyweapon;
    int pendingweapon;
    int weaponowned[9];
    int ammo[4];
    int maxammo[4];
    int cards[6];
    int powers[6];
    int backpack;
    int kills;
    int items;
    int secrets;
    int damagecount;
    int bonuscount;
    int has_attacker;
    int attacker_type;
    double attacker_bearing;
    double x, y, z;
    double angle;
} dmcp_state_t;

typedef struct
{
    int64_t id;
    int type;
    int flags;
    int health;
    int line_of_sight;
    int targeting_player;
    double x, y, z;
    double distance;
    double bearing;
    double radius;
} dmcp_thing_t;

typedef struct
{
    double x1, y1, x2, y2;
    int flags;
    int special;
    int two_sided;
    double front_floor, front_ceiling;
    double back_floor, back_ceiling;
    int front_sector;
    int back_sector; // -1 when one-sided
    int tag;
} dmcp_line_t;

typedef struct
{
    double floor, ceiling;
    int special;
    int tag;
    int moving; // a door/floor/lift/ceiling is currently moving in it
    int floorpic;
} dmcp_sector_t;

#endif
