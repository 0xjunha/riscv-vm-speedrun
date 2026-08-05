#include "rvb_workload_common.h"
#include "sglib.h"

#include <stddef.h>
#include <stdint.h>

#define MAX_VALUES 384
#define QUEUE_CAPACITY (MAX_VALUES + 1)
#define HASH_SIZE 31

struct list_node {
    int value;
    struct list_node *previous;
    struct list_node *next;
};
typedef struct list_node list_node;

static int compare_values(int left, int right) {
    return (left > right) - (left < right);
}

#define LIST_COMPARE(left, right)                                            \
    compare_values((left)->value, (right)->value)
SGLIB_DEFINE_DL_LIST_PROTOTYPES(list_node, LIST_COMPARE, previous, next)
SGLIB_DEFINE_DL_LIST_FUNCTIONS(list_node, LIST_COMPARE, previous, next)

struct hash_node {
    int value;
    struct hash_node *next;
};
typedef struct hash_node hash_node;

#define HASH_COMPARE(left, right)                                            \
    compare_values((left)->value, (right)->value)
static unsigned int hash_value(struct hash_node *node) {
    return (unsigned int)node->value;
}
SGLIB_DEFINE_LIST_PROTOTYPES(hash_node, HASH_COMPARE, next)
SGLIB_DEFINE_LIST_FUNCTIONS(hash_node, HASH_COMPARE, next)
SGLIB_DEFINE_HASHED_CONTAINER_PROTOTYPES(hash_node, HASH_SIZE, hash_value)
SGLIB_DEFINE_HASHED_CONTAINER_FUNCTIONS(hash_node, HASH_SIZE, hash_value)

struct tree_node {
    int value;
    char color;
    struct tree_node *left;
    struct tree_node *right;
};
typedef struct tree_node tree_node;

#define TREE_COMPARE(left, right)                                            \
    compare_values((left)->value, (right)->value)
SGLIB_DEFINE_RBTREE_PROTOTYPES(tree_node, left, right, color, TREE_COMPARE)
SGLIB_DEFINE_RBTREE_FUNCTIONS(tree_node, left, right, color, TREE_COMPARE)

uint32_t rvb_sglib(const uint8_t *input, uint32_t input_len,
                   uint32_t out[2]) {
    if (input_len < 16u * 4u || input_len % 4u != 0u ||
        input_len / 4u > MAX_VALUES) {
        return RVB_BAD_INPUT;
    }
    const int count = (int)(input_len / 4u);
    int values[MAX_VALUES];
    int sorted[MAX_VALUES];
    struct list_node list_nodes[MAX_VALUES];
    struct hash_node hash_nodes[MAX_VALUES];
    struct tree_node tree_nodes[MAX_VALUES];

    for (int i = 0; i < count; ++i) {
        values[i] = (int32_t)rvb_read_u32(input + (uint32_t)i * 4u);
        sorted[i] = values[i];
    }
    SGLIB_ARRAY_SINGLE_QUICK_SORT(int, sorted, count, SGLIB_NUMERIC_COMPARATOR);

    uint32_t ordered = 0x53474c49u;
    for (int i = 0; i < count; ++i) {
        ordered = rvb_fold(ordered, (uint32_t)sorted[i], (uint32_t)i);
    }

    struct list_node *list = NULL;
    for (int i = 0; i < count; ++i) {
        list_nodes[i].value = values[i];
        sglib_list_node_add(&list, &list_nodes[i]);
    }
    sglib_list_node_sort(&list);
    uint32_t index = (uint32_t)count;
    for (struct list_node *node = sglib_list_node_get_first(list); node != NULL;
         node = node->next) {
        ordered = rvb_fold(ordered, (uint32_t)node->value, index++);
    }

    struct hash_node *table[HASH_SIZE];
    sglib_hashed_hash_node_init(table);
    int unique_count = 0;
    for (int i = 0; i < count; ++i) {
        struct hash_node probe = {values[i], NULL};
        if (sglib_hashed_hash_node_find_member(table, &probe) == NULL) {
            hash_nodes[unique_count].value = values[i];
            sglib_hashed_hash_node_add(table, &hash_nodes[unique_count]);
            ++unique_count;
        }
    }

    struct tree_node *tree = NULL;
    int tree_count = 0;
    for (int i = 0; i < count; ++i) {
        struct tree_node probe = {values[i], 0, NULL, NULL};
        if (sglib_tree_node_find_member(tree, &probe) == NULL) {
            tree_nodes[tree_count].value = values[i];
            sglib_tree_node_add(&tree, &tree_nodes[tree_count]);
            ++tree_count;
        }
    }
    struct sglib_tree_node_iterator tree_iterator;
    struct tree_node *tree_node =
        sglib_tree_node_it_init_inorder(&tree_iterator, tree);
    while (tree_node != NULL) {
        ordered = rvb_fold(ordered, (uint32_t)tree_node->value, index++);
        tree_node = sglib_tree_node_it_next(&tree_iterator);
    }

    int queue[QUEUE_CAPACITY];
    int first;
    int last;
    SGLIB_QUEUE_INIT(int, queue, first, last);
    for (int i = 0; i < count; ++i) {
        SGLIB_QUEUE_ADD(int, queue, values[i], first, last, QUEUE_CAPACITY);
    }
    uint32_t operations = 0x434f4e54u;
    index = 0u;
    while (!SGLIB_QUEUE_IS_EMPTY(int, queue, first, last)) {
        operations = rvb_fold(
            operations,
            (uint32_t)SGLIB_QUEUE_FIRST_ELEMENT(int, queue, first, last),
            index++);
        SGLIB_QUEUE_DELETE(int, queue, first, last, QUEUE_CAPACITY);
    }

    int heap[MAX_VALUES];
    int heap_size;
    SGLIB_HEAP_INIT(int, heap, heap_size);
    for (int i = 0; i < count; ++i) {
        SGLIB_HEAP_ADD(int, heap, values[i], heap_size, MAX_VALUES,
                       SGLIB_NUMERIC_COMPARATOR);
    }
    while (!SGLIB_HEAP_IS_EMPTY(int, heap, heap_size)) {
        operations = rvb_fold(
            operations, (uint32_t)SGLIB_HEAP_FIRST_ELEMENT(int, heap, heap_size),
            index++);
        SGLIB_HEAP_DELETE(int, heap, heap_size, MAX_VALUES,
                          SGLIB_NUMERIC_COMPARATOR);
    }
    operations = rvb_fold(operations, (uint32_t)unique_count, index);

    out[0] = ordered;
    out[1] = operations;
    return 0u;
}
